//! The echo canceller's saved states in `~/Library/Caches/mic-dsp`, so that the first call after
//! micd starts does not let echo through while the canceller relearns the room. Control thread
//! only.
//!
//! There are two, because the owner listens on open headphones or on speakers and the two echo
//! paths have nothing in common: `echo-headphones.bin` and `echo-speakers.bin`. `echo-last`
//! names the one used last. Nothing the Komplete reports tells which setup is in use, so the
//! echo does: a session's state is filed under the setup its echo level says, and a session
//! starts by trying the other slot's state against the canceller in use (`dsp::Chain`).
//!
//! The files are a cache. Without them, or with ones this build cannot read, micd runs as it
//! does without this module, and the next session writes new ones.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use dsp::{Chain, EchoStatus, EchoTrial};

/// On open headphones the echo is at least this far under what plays, in dB, and on speakers
/// it is closer. The recordings read 54 and 18 (`EchoStatus::echo_level_db`).
const HEADPHONES_DB: f32 = 30.0;
/// A session's state is saved once it had this many frames of far end alone: 5 s. The
/// canceller has left AEC3's initial phase by then, and the echo level is measured.
const MIN_FAR_FRAMES: u64 = 500;
const MARKER: &str = "echo-last";

/// The setup a saved state was learned in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    Headphones,
    Speakers,
}

impl Slot {
    /// The setup in which the echo is `echo_level_db` under what plays.
    pub fn of(echo_level_db: f32) -> Slot {
        if echo_level_db >= HEADPHONES_DB {
            Slot::Headphones
        } else {
            Slot::Speakers
        }
    }

    pub fn other(self) -> Slot {
        match self {
            Slot::Headphones => Slot::Speakers,
            Slot::Speakers => Slot::Headphones,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Slot::Headphones => "headphones",
            Slot::Speakers => "speakers",
        }
    }

    fn file(self) -> String {
        format!("echo-{}.bin", self.name())
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What a mic session started with.
#[derive(Clone, Copy, Default)]
pub struct Session {
    /// The setup whose state the canceller in use has, when that is known.
    pub current: Option<Slot>,
    /// The setup whose state the session tries against it.
    pub candidate: Option<Slot>,
    /// The chain's echo status when the session started.
    pub echo: EchoStatus,
    /// The chain was built for this session.
    pub new_chain: bool,
}

impl Session {
    /// What the log says when the engine starts.
    pub fn describe(&self) -> String {
        let canceller = match (self.new_chain, self.current) {
            (true, Some(slot)) => format!("{slot} loaded into the new chain"),
            (true, None) => {
                return "echo presets: none to load, so the new chain's canceller has yet to \
                        learn the room"
                    .to_owned()
            }
            (false, Some(slot)) => format!("the canceller in memory has {slot}"),
            (false, None) => {
                return "echo presets: none yet, and the canceller in memory goes on".to_owned()
            }
        };
        match (self.candidate, self.current) {
            (Some(candidate), _) => {
                format!("echo presets: {canceller}, and {candidate} is tried against it")
            }
            (None, Some(slot)) => format!(
                "echo presets: {canceller}, with no {} preset to try against it",
                slot.other()
            ),
            (None, None) => format!("echo presets: {canceller}"),
        }
    }

    /// The status line's part about the session's trial of two presets, when it had one.
    pub fn describe_trial(&self, echo: &EchoStatus) -> Option<String> {
        let (current, candidate) = (self.current?, self.candidate?);
        // The level of the preset that stays comes first, as its name does.
        let levels = |switched: bool| {
            echo.trial_levels_dbfs()
                .map_or(String::new(), |[current, tried]| {
                    let (won, lost) = if switched { (tried, current) } else { (current, tried) };
                    format!(" ({won:.1} against {lost:.1} dBFS out)")
                })
        };
        match echo.trial {
            EchoTrial::None => None,
            EchoTrial::Running => Some(format!("trying the {candidate} preset against {current}")),
            EchoTrial::Kept => Some(format!(
                "kept the {current} preset over {candidate}{}",
                levels(false)
            )),
            EchoTrial::Switched => Some(format!(
                "took the {candidate} preset over {current}{}",
                levels(true)
            )),
        }
    }
}

/// The cache directory and what was logged about it.
pub struct Presets {
    dir: PathBuf,
    /// What was last logged about each slot's file, so that a file that stays missing or
    /// unreadable is logged once.
    noted: [Option<String>; 2],
}

/// `~/Library/Caches/mic-dsp`. It is created when the first state is saved.
pub fn dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    Ok(Path::new(&home).join("Library/Caches/mic-dsp"))
}

impl Presets {
    pub fn new(dir: PathBuf) -> Presets {
        Presets {
            dir,
            noted: [None, None],
        }
    }

    /// Gives a new chain the state of the setup used last, or the other one's when that is
    /// not there. Returns the setup whose state it has.
    pub fn load(&mut self, chain: &mut Chain) -> Option<Slot> {
        let last = self.last();
        [last, last.other()].into_iter().find(|slot| {
            self.read(*slot)
                .is_some_and(|state| self.accepted(*slot, chain.load_echo_preset(&state)))
        })
    }

    /// Starts a session on the chain, with the other setup's state as the candidate when
    /// there is one. Returns that setup.
    pub fn start(&mut self, chain: &mut Chain, current: Option<Slot>) -> Option<Slot> {
        let candidate = current.map(Slot::other).filter(|slot| {
            self.read(*slot)
                .is_some_and(|state| self.accepted(*slot, chain.start_echo_session(Some(&state))))
        });
        if candidate.is_none() {
            // No trial. The session's measurements still start over.
            let _ = chain.start_echo_session(None);
        }
        candidate
    }

    /// Ends a session: saves the canceller's state under the setup its echo level says, once
    /// it heard 5 s of far end alone. Returns the setup whose state the canceller has now,
    /// and what to log.
    pub fn finish(
        &mut self,
        chain: &Chain,
        session: &Session,
        echo: &EchoStatus,
    ) -> (Option<Slot>, Option<String>) {
        // After a trial that switched, the canceller in use is the candidate's.
        let mut current = match echo.trial {
            EchoTrial::Switched => session.candidate.or(session.current),
            _ => session.current,
        };
        let far = echo.far_frames.saturating_sub(session.echo.far_frames);
        let seconds = far as f64 / 100.0;
        let needed = MIN_FAR_FRAMES / 100;
        let mut saved = false;
        let message = match echo.echo_level_db {
            Some(level) if far >= MIN_FAR_FRAMES => {
                let slot = Slot::of(level);
                match chain.echo_state().map(|state| self.write(slot, &state)) {
                    Some(Ok(())) => {
                        saved = true;
                        current = Some(slot);
                        Some(format!(
                            "saved the echo canceller's state as the {slot} preset: the echo \
                             was {level:.1} dB under what played, over {seconds:.0} s of far \
                             end alone"
                        ))
                    }
                    Some(Err(error)) => Some(format!(
                        "could not save the {slot} echo preset in {}: {error}",
                        self.dir.display()
                    )),
                    None => Some(
                        "echo preset not saved: the canceller is in its initial phase again"
                            .to_owned(),
                    ),
                }
            }
            _ if far > 0 => Some(format!(
                "echo preset not saved: {seconds:.1} s of far end alone, and it takes {needed}"
            )),
            _ => None,
        };
        // The marker follows a trial that switched, with or without a save.
        if let (false, Some(slot)) = (saved || current == session.current, current) {
            if let Err(error) = self.mark(slot) {
                log!(
                    "could not write {}: {error}",
                    self.dir.join(MARKER).display()
                );
            }
        }
        (current, message)
    }

    /// The setup used last. Headphones, the usual one, when nothing says.
    fn last(&self) -> Slot {
        match fs::read_to_string(self.dir.join(MARKER)) {
            Ok(name) if name.trim() == Slot::Speakers.name() => Slot::Speakers,
            _ => Slot::Headphones,
        }
    }

    fn read(&mut self, slot: Slot) -> Option<Vec<u8>> {
        let path = self.dir.join(slot.file());
        match fs::read(&path) {
            Ok(state) => Some(state),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.note(
                    slot,
                    format!("no {slot} echo preset yet ({})", path.display()),
                );
                None
            }
            Err(error) => {
                self.note(slot, format!("cannot read {}: {error}", path.display()));
                None
            }
        }
    }

    /// Whether the chain took the slot's state. A state it refused is logged, once.
    fn accepted(&mut self, slot: Slot, loaded: Result<(), dsp::EchoStateError>) -> bool {
        match loaded {
            Ok(()) => {
                self.noted[slot as usize] = None;
                true
            }
            Err(error) => {
                let path = self.dir.join(slot.file());
                self.note(slot, format!("not using {}: {error}", path.display()));
                false
            }
        }
    }

    fn note(&mut self, slot: Slot, message: String) {
        let noted = &mut self.noted[slot as usize];
        if noted.as_ref() != Some(&message) {
            log!("{message}");
            *noted = Some(message);
        }
    }

    fn write(&mut self, slot: Slot, state: &[u8]) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        write_atomically(&self.dir.join(slot.file()), state)?;
        self.mark(slot)
    }

    fn mark(&self, slot: Slot) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        write_atomically(&self.dir.join(MARKER), format!("{slot}\n").as_bytes())
    }
}

/// Writes a file so that a reader, or a micd that starts after a crash, finds the old contents
/// or the new and never part of either: into a temporary file next to it, flushed to disk,
/// then renamed over it.
fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let written = File::create(&temporary).and_then(|mut file| {
        file.write_all(contents)?;
        file.sync_all()
    });
    let renamed = written.and_then(|()| fs::rename(&temporary, path));
    if renamed.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    renamed
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsp::Config;
    use std::os::unix::fs::MetadataExt;

    /// A fresh directory under the system's temporary directory.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("micd-presets-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_echo_level_says_which_setup_it_is() {
        assert_eq!(Slot::of(30.0), Slot::Headphones);
        assert_eq!(Slot::of(29.9), Slot::Speakers);
        // The recordings: 54 dB on open headphones, 42 with the owner talking over them, and
        // 18 on speakers.
        assert_eq!(Slot::of(54.0), Slot::Headphones);
        assert_eq!(Slot::of(42.0), Slot::Headphones);
        assert_eq!(Slot::of(17.9), Slot::Speakers);
        assert_eq!(Slot::Headphones.other(), Slot::Speakers);
        assert_eq!(Slot::Speakers.other(), Slot::Headphones);
        assert_eq!(Slot::Headphones.file(), "echo-headphones.bin");
        assert_eq!(Slot::Speakers.file(), "echo-speakers.bin");
    }

    #[test]
    fn a_state_is_written_whole_and_read_back() {
        let dir = scratch("write");
        let mut presets = Presets::new(dir.join("nested/mic-dsp"));
        let dir = dir.join("nested/mic-dsp");
        // Nothing there: headphones are the usual setup, and there is nothing to read.
        assert_eq!(presets.last(), Slot::Headphones);
        assert_eq!(presets.read(Slot::Speakers), None);
        assert!(!dir.exists(), "reading creates nothing");

        let first = vec![7u8; 50_000];
        presets.write(Slot::Speakers, &first).unwrap();
        assert_eq!(files(&dir), ["echo-last", "echo-speakers.bin"]);
        assert_eq!(presets.read(Slot::Speakers), Some(first));
        assert_eq!(presets.read(Slot::Headphones), None);
        assert_eq!(presets.last(), Slot::Speakers);
        assert_eq!(fs::read_to_string(dir.join(MARKER)).unwrap(), "speakers\n");

        // A new state replaces the file by a rename: it is another file under the same name,
        // so nobody ever sees half of it. What a crashed write left behind is written over.
        let path = dir.join("echo-speakers.bin");
        let inode = fs::metadata(&path).unwrap().ino();
        fs::write(dir.join("echo-speakers.bin.tmp"), b"half a st").unwrap();
        let second: Vec<u8> = (0..60_000u32).map(|i| i as u8).collect();
        presets.write(Slot::Speakers, &second).unwrap();
        assert_ne!(fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(presets.read(Slot::Speakers), Some(second.clone()));
        assert_eq!(files(&dir), ["echo-last", "echo-speakers.bin"]);

        presets.write(Slot::Headphones, &[1, 2, 3]).unwrap();
        assert_eq!(presets.last(), Slot::Headphones);
        assert_eq!(
            files(&dir),
            ["echo-headphones.bin", "echo-last", "echo-speakers.bin"]
        );
        assert_eq!(presets.read(Slot::Speakers), Some(second));
        presets.mark(Slot::Speakers).unwrap();
        assert_eq!(presets.last(), Slot::Speakers);

        // A marker that says nothing known means the usual setup.
        fs::write(dir.join(MARKER), "loudspeakers").unwrap();
        assert_eq!(presets.last(), Slot::Headphones);
        let _ = fs::remove_dir_all(scratch("write"));
    }

    #[test]
    fn a_write_that_fails_leaves_the_old_state() {
        let dir = scratch("fail");
        let mut presets = Presets::new(dir.clone());
        presets.write(Slot::Speakers, b"the old state").unwrap();
        // A directory where the temporary file goes makes the write fail before the rename.
        fs::create_dir(dir.join("echo-speakers.bin.tmp")).unwrap();
        assert!(presets.write(Slot::Speakers, b"the new state").is_err());
        assert_eq!(
            presets.read(Slot::Speakers),
            Some(b"the old state".to_vec())
        );
        // And a cache directory that cannot be made is an error, not a panic.
        let mut nowhere = Presets::new(dir.join("echo-last/under-a-file"));
        assert!(nowhere.write(Slot::Headphones, b"state").is_err());
        assert_eq!(nowhere.read(Slot::Headphones), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_problem_with_a_file_is_noted_once() {
        let dir = scratch("note");
        let mut presets = Presets::new(dir.clone());
        assert_eq!(presets.read(Slot::Speakers), None);
        let first = presets.noted[Slot::Speakers as usize].clone().unwrap();
        assert!(
            first.starts_with("no speakers echo preset yet ("),
            "{first}"
        );
        assert_eq!(presets.noted[Slot::Headphones as usize], None);
        assert_eq!(presets.read(Slot::Speakers), None);
        assert_eq!(presets.noted[Slot::Speakers as usize], Some(first));

        // A file the chain refuses is noted and left where it is.
        presets.write(Slot::Speakers, b"not a state").unwrap();
        let (mut chain, _control) = dsp::chain(&Config::default());
        assert_eq!(presets.load(&mut chain), None);
        let refused = presets.noted[Slot::Speakers as usize].clone().unwrap();
        assert!(
            refused.contains("echo-speakers.bin: not a saved"),
            "{refused}"
        );
        assert_eq!(presets.start(&mut chain, Some(Slot::Headphones)), None);
        assert_eq!(presets.noted[Slot::Speakers as usize], Some(refused));
        assert!(dir.join("echo-speakers.bin").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_log_says_which_presets_a_session_starts_with() {
        let session = |new_chain, current, candidate| Session {
            current,
            candidate,
            echo: EchoStatus::default(),
            new_chain,
        };
        let (headphones, speakers) = (Some(Slot::Headphones), Some(Slot::Speakers));
        assert_eq!(
            session(true, headphones, speakers).describe(),
            "echo presets: headphones loaded into the new chain, and speakers is tried against it"
        );
        assert_eq!(
            session(true, speakers, None).describe(),
            "echo presets: speakers loaded into the new chain, with no headphones preset to try \
             against it"
        );
        assert_eq!(
            session(true, None, None).describe(),
            "echo presets: none to load, so the new chain's canceller has yet to learn the room"
        );
        assert_eq!(
            session(false, speakers, headphones).describe(),
            "echo presets: the canceller in memory has speakers, and headphones is tried against \
             it"
        );
        assert_eq!(
            session(false, headphones, None).describe(),
            "echo presets: the canceller in memory has headphones, with no speakers preset to \
             try against it"
        );
        assert_eq!(
            session(false, None, None).describe(),
            "echo presets: none yet, and the canceller in memory goes on"
        );
    }

    #[test]
    fn the_status_line_says_which_preset_won() {
        let session = Session {
            current: Some(Slot::Headphones),
            candidate: Some(Slot::Speakers),
            echo: EchoStatus::default(),
            new_chain: true,
        };
        let mut echo = EchoStatus {
            trial: EchoTrial::Running,
            ..EchoStatus::default()
        };
        assert_eq!(
            session.describe_trial(&echo).unwrap(),
            "trying the speakers preset against headphones"
        );
        // 25 frames at -48.3 dBFS out of the canceller in use and -72.0 out of the other.
        let energy = |dbfs: f64| 25.0 * 480.0 * 10f64.powf(dbfs / 10.0);
        echo.trial_frames = 25;
        echo.trial_out = [energy(-48.3), energy(-72.0)];
        echo.trial = EchoTrial::Switched;
        assert_eq!(
            session.describe_trial(&echo).unwrap(),
            "took the speakers preset over headphones (-72.0 against -48.3 dBFS out)"
        );
        echo.trial_out = [energy(-73.5), energy(-73.1)];
        echo.trial = EchoTrial::Kept;
        assert_eq!(
            session.describe_trial(&echo).unwrap(),
            "kept the headphones preset over speakers (-73.5 against -73.1 dBFS out)"
        );
        // A session without a second preset has nothing to say.
        echo.trial = EchoTrial::None;
        assert_eq!(session.describe_trial(&echo), None);
        let alone = Session {
            candidate: None,
            ..session
        };
        assert_eq!(alone.describe_trial(&echo), None);
    }

    /// A far end of noise in bursts and the mic that hears it 10 ms later at `gain` over
    /// faint noise, run through the chain as one mic session.
    fn call(chain: &mut Chain, seconds: usize, gain: f32) {
        let mut seed = 0x1234_5678u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1 << 23) as f32 - 1.0
        };
        let mut low = 0.0f32;
        let far: Vec<f32> = (0..seconds * 48_000)
            .map(|i| {
                low += 0.3 * (noise() - low);
                if (i / 28_800) % 5 < 4 {
                    0.5 * low
                } else {
                    0.0
                }
            })
            .collect();
        let mic: Vec<f32> = (0..far.len())
            .map(|i| 0.0005 * noise() + i.checked_sub(480).map_or(0.0, |j| gain * far[j]))
            .collect();
        let mut output = [0.0f32; 512];
        for (x, r) in mic.chunks(512).zip(far.chunks(512)) {
            chain.process_with_reference(x, 1, r, 1, &mut output[..x.len()], 1);
        }
    }

    /// Sessions on speakers and on headphones, across a restart of micd: each state lands in
    /// its slot, the last one used comes back as the canceller, and the other is tried.
    #[test]
    fn states_are_filed_by_echo_level_and_come_back_after_a_restart() {
        let dir = scratch("sessions");
        let mut presets = Presets::new(dir.clone());

        // The first session ever: nothing to load, nothing to try. It is on speakers.
        let (mut chain, mut control) = dsp::chain(&Config::default());
        let mut session = Session {
            current: presets.load(&mut chain),
            new_chain: true,
            ..Session::default()
        };
        session.candidate = presets.start(&mut chain, session.current);
        session.echo = control.status().echo;
        assert_eq!((session.current, session.candidate), (None, None));

        // Too short to save: 1.6 s of far end.
        call(&mut chain, 2, 0.3);
        let (slot, message) = presets.finish(&chain, &session, &control.status().echo);
        assert_eq!(slot, None);
        let message = message.unwrap();
        assert!(
            message.starts_with("echo preset not saved: ") && message.ends_with("it takes 5"),
            "{message}"
        );
        assert!(!dir.exists());

        call(&mut chain, 9, 0.3);
        let echo = control.status().echo;
        let (slot, message) = presets.finish(&chain, &session, &echo);
        assert_eq!(slot, Some(Slot::Speakers));
        let message = message.unwrap();
        assert!(
            message.starts_with("saved the echo canceller's state as the speakers preset: "),
            "{message}"
        );
        assert_eq!(files(&dir), ["echo-last", "echo-speakers.bin"]);
        let speakers = fs::read(dir.join("echo-speakers.bin")).unwrap();
        assert!(chain.echo_state() == Some(speakers.clone()));

        // The next session on the same chain is on headphones, where the echo is no louder
        // than the room. There is no headphones state to try yet.
        session = Session {
            current: slot,
            candidate: presets.start(&mut chain, slot),
            echo: control.status().echo,
            new_chain: false,
        };
        assert_eq!(session.candidate, None);
        assert_eq!(control.status().echo.echo_level_db, None);
        call(&mut chain, 9, 0.003);
        let (slot, message) = presets.finish(&chain, &session, &control.status().echo);
        assert_eq!(slot, Some(Slot::Headphones), "{message:?}");
        assert_eq!(
            files(&dir),
            ["echo-headphones.bin", "echo-last", "echo-speakers.bin"]
        );
        assert!(fs::read(dir.join("echo-speakers.bin")).unwrap() == speakers);
        drop((chain, control));

        // micd restarts. Headphones were last, so that state is the canceller's, and the
        // speakers state is tried against it. The call is on speakers, and the trial says so.
        let mut presets = Presets::new(dir.clone());
        let (mut chain, mut control) = dsp::chain(&Config::default());
        let current = presets.load(&mut chain);
        let session = Session {
            current,
            candidate: presets.start(&mut chain, current),
            echo: control.status().echo,
            new_chain: true,
        };
        assert_eq!(
            (session.current, session.candidate),
            (Some(Slot::Headphones), Some(Slot::Speakers))
        );
        assert_eq!(control.status().echo.trial, EchoTrial::Running);
        assert!(chain.echo_state() == Some(fs::read(dir.join("echo-headphones.bin")).unwrap()));

        // Too short to save, and long enough for the trial: the marker follows the switch.
        call(&mut chain, 3, 0.3);
        let echo = control.status().echo;
        assert_eq!(echo.trial, EchoTrial::Switched, "{echo:?}");
        assert!(echo.far_frames < MIN_FAR_FRAMES);
        let (slot, _) = presets.finish(&chain, &session, &echo);
        assert_eq!(slot, Some(Slot::Speakers));
        assert_eq!(presets.last(), Slot::Speakers);
        assert!(fs::read(dir.join("echo-speakers.bin")).unwrap() == speakers);
        let _ = fs::remove_dir_all(&dir);
    }
}
