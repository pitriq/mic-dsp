//! Saving and loading what a canceller has learned (not in WebRTC).
//!
//! A saved state is every value the canceller carries from one block to the next, apart from
//! its signal histories: the render buffer, the filter banks' memories, the framing, the
//! windows' and the overlap-add's previous blocks, and the suppressor's 16 ms average of the
//! near end. A loaded canceller starts with those as a new one does, and with everything else
//! as the saved one had it. So it is past AEC3's initial phase if the saved one was, and its
//! filters, gains and estimators carry on where they stopped.
//!
//! The format is written by hand: little-endian values in the order each module lists its
//! fields, after a header that pins the format and the sizes of the arrays, and before a
//! checksum.
//!
//! ```text
//! magic         8 bytes   "AEC3STAT"
//! version       u32
//! length        u32       of the whole state, the checksum included
//! filters       4 x u32   refined, coarse, refined initial and coarse initial lengths, in blocks
//! fields        ...       f32 and i32 as 4 bytes, counts as u64, flags as one byte
//! checksum      u64       FNV-1a of everything before it
//! ```
//!
//! Saving and loading allocate. They belong on a control thread.

use std::fmt;

use crate::fft_data::FftData;

const MAGIC: [u8; 8] = *b"AEC3STAT";
/// Changes whenever a module's list of fields does.
const VERSION: u32 = 1;
const HEADER: usize = MAGIC.len() + 4 + 4 + 4 * 4;
const CHECKSUM: usize = 8;

/// Why a saved state was not loaded. The canceller is as it was before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateError {
    /// The bytes do not start as a saved state does.
    NotAState,
    /// Saved by another version of the format.
    Version(u32),
    /// Saved by a canceller with other filter lengths.
    Config,
    /// Shorter than its header says.
    Truncated,
    /// The checksum does not match, or a value is one no canceller holds.
    Corrupt,
    /// Saved before the canceller had left AEC3's initial phase, so there is nothing in it to
    /// start from.
    Untrained,
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StateError::NotAState => f.write_str("not a saved echo canceller state"),
            StateError::Version(found) => write!(
                f,
                "saved in format version {found}, and this build reads version {VERSION}"
            ),
            StateError::Config => f.write_str("saved with other filter lengths"),
            StateError::Truncated => f.write_str("truncated"),
            StateError::Corrupt => f.write_str("corrupt"),
            StateError::Untrained => {
                f.write_str("saved before the canceller had left its initial phase")
            }
        }
    }
}

impl std::error::Error for StateError {}

/// The filter lengths that set the sizes of the saved arrays: refined, coarse, refined
/// initial, coarse initial.
pub(crate) type Sizes = [usize; 4];

pub(crate) struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    /// Starts a state with its header.
    pub fn new(sizes: Sizes) -> Writer {
        let mut bytes = Vec::with_capacity(64 * 1024);
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        // The length, filled in by `finish`.
        bytes.extend_from_slice(&[0; 4]);
        for size in sizes {
            bytes.extend_from_slice(&(size as u32).to_le_bytes());
        }
        Writer { bytes }
    }

    fn put(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// Fills in the length and appends the checksum.
    pub fn finish(mut self) -> Vec<u8> {
        let length = (self.bytes.len() + CHECKSUM) as u32;
        self.bytes[12..16].copy_from_slice(&length.to_le_bytes());
        let checksum = fnv1a(&self.bytes);
        self.put(&checksum.to_le_bytes());
        self.bytes
    }
}

pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    /// Checks the header and the checksum of a state saved with `sizes`, and returns a reader
    /// at its first field.
    pub fn new(bytes: &'a [u8], sizes: Sizes) -> Result<Reader<'a>, StateError> {
        let Some(magic) = bytes.get(..MAGIC.len()) else {
            return Err(StateError::NotAState);
        };
        if magic != MAGIC {
            return Err(StateError::NotAState);
        }
        if bytes.len() < HEADER + CHECKSUM {
            return Err(StateError::Truncated);
        }
        let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        if word(8) != VERSION {
            return Err(StateError::Version(word(8)));
        }
        let length = word(12) as usize;
        if bytes.len() < length {
            return Err(StateError::Truncated);
        }
        if bytes.len() > length || length < HEADER + CHECKSUM {
            return Err(StateError::Corrupt);
        }
        let (body, checksum) = bytes.split_at(length - CHECKSUM);
        if fnv1a(body).to_le_bytes() != checksum {
            return Err(StateError::Corrupt);
        }
        for (i, size) in sizes.into_iter().enumerate() {
            if word(16 + 4 * i) as usize != size {
                return Err(StateError::Config);
            }
        }
        Ok(Reader {
            bytes: &body[HEADER..],
        })
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], StateError> {
        let (head, rest) = self
            .bytes
            .split_first_chunk::<N>()
            .ok_or(StateError::Corrupt)?;
        self.bytes = rest;
        Ok(*head)
    }

    /// Every field was read: nothing is left before the checksum.
    pub fn finish(self) -> Result<(), StateError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(StateError::Corrupt)
        }
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// `Err(Corrupt)` unless `valid`: a loaded value that no canceller holds.
pub(crate) fn check(valid: bool) -> Result<(), StateError> {
    if valid {
        Ok(())
    } else {
        Err(StateError::Corrupt)
    }
}

/// A value, or a module's fields, in a saved state.
pub(crate) trait State {
    fn save(&self, w: &mut Writer);
    /// Reads what `save` wrote. After an error the value is half loaded: `EchoCanceller::load`
    /// loads into a canceller of its own and drops it.
    fn load(&mut self, r: &mut Reader<'_>) -> Result<(), StateError>;
}

/// What an absent `Option` is loaded into.
pub(crate) trait Blank {
    const BLANK: Self;
}

impl State for f32 {
    fn save(&self, w: &mut Writer) {
        w.put(&self.to_le_bytes());
    }

    /// A canceller's state has no infinity and no NaN in it, and one that was loaded would
    /// never get rid of it.
    fn load(&mut self, r: &mut Reader<'_>) -> Result<(), StateError> {
        *self = f32::from_le_bytes(r.take()?);
        check(self.is_finite())
    }
}

impl State for i32 {
    fn save(&self, w: &mut Writer) {
        w.put(&self.to_le_bytes());
    }

    fn load(&mut self, r: &mut Reader<'_>) -> Result<(), StateError> {
        *self = i32::from_le_bytes(r.take()?);
        Ok(())
    }
}

impl State for u32 {
    fn save(&self, w: &mut Writer) {
        w.put(&self.to_le_bytes());
    }

    fn load(&mut self, r: &mut Reader<'_>) -> Result<(), StateError> {
        *self = u32::from_le_bytes(r.take()?);
        Ok(())
    }
}

impl State for usize {
    fn save(&self, w: &mut Writer) {
        w.put(&(*self as u64).to_le_bytes());
    }

    fn load(&mut self, r: &mut Reader<'_>) -> Result<(), StateError> {
        *self = usize::try_from(u64::from_le_bytes(r.take()?)).map_err(|_| StateError::Corrupt)?;
        Ok(())
    }
}

impl State for bool {
    fn save(&self, w: &mut Writer) {
        w.put(&[u8::from(*self)]);
    }

    fn load(&mut self, r: &mut Reader<'_>) -> Result<(), StateError> {
        let [byte] = r.take()?;
        check(byte <= 1)?;
        *self = byte == 1;
        Ok(())
    }
}

impl Blank for f32 {
    const BLANK: f32 = 0.0;
}

impl Blank for usize {
    const BLANK: usize = 0;
}

impl<T: Blank, const N: usize> Blank for [T; N] {
    const BLANK: [T; N] = [T::BLANK; N];
}

impl<T: State, const N: usize> State for [T; N] {
    fn save(&self, w: &mut Writer) {
        self.iter().for_each(|value| value.save(w));
    }

    fn load(&mut self, r: &mut Reader<'_>) -> Result<(), StateError> {
        self.iter_mut().try_for_each(|value| value.load(r))
    }
}

/// The elements alone. The length is the one the canceller was built with, which the header's
/// filter lengths pin. A vector whose length changes while the canceller runs goes through
/// `save_resizable`.
impl<T: State> State for Vec<T> {
    fn save(&self, w: &mut Writer) {
        self.iter().for_each(|value| value.save(w));
    }

    fn load(&mut self, r: &mut Reader<'_>) -> Result<(), StateError> {
        self.iter_mut().try_for_each(|value| value.load(r))
    }
}

impl<T: State + Blank> State for Option<T> {
    fn save(&self, w: &mut Writer) {
        self.is_some().save(w);
        if let Some(value) = self {
            value.save(w);
        }
    }

    fn load(&mut self, r: &mut Reader<'_>) -> Result<(), StateError> {
        let mut present = false;
        present.load(r)?;
        *self = None;
        if present {
            self.insert(T::BLANK).load(r)?;
        }
        Ok(())
    }
}

impl State for FftData {
    fn save(&self, w: &mut Writer) {
        self.re.save(w);
        self.im.save(w);
    }

    fn load(&mut self, r: &mut Reader<'_>) -> Result<(), StateError> {
        self.re.load(r)?;
        self.im.load(r)
    }
}

/// A vector that the canceller resizes within its capacity: the length, then the elements.
pub(crate) fn save_resizable(values: &[f32], w: &mut Writer) {
    values.len().save(w);
    values.iter().for_each(|value| value.save(w));
}

/// Loads what `save_resizable` wrote. The length is a whole number of `unit`s, at least one,
/// and within the capacity the canceller allocated.
pub(crate) fn load_resizable(
    values: &mut Vec<f32>,
    unit: usize,
    r: &mut Reader<'_>,
) -> Result<(), StateError> {
    let mut len = 0usize;
    len.load(r)?;
    check(len >= unit && len.is_multiple_of(unit) && len <= values.capacity())?;
    values.resize(len, 0.0);
    values.load(r)
}

/// Implements `State` for a module's struct as the fields listed, in that order. A field may
/// be a path into a nested struct. `valid` is a condition the loaded fields must meet.
macro_rules! persist {
    ($ty:ty { $( $($field:ident).+ ),* $(,)? } $(valid: |$this:ident| $valid:expr)?) => {
        impl $crate::state::State for $ty {
            fn save(&self, w: &mut $crate::state::Writer) {
                $( $crate::state::State::save(&self.$($field).+, w); )*
            }

            fn load(
                &mut self,
                r: &mut $crate::state::Reader<'_>,
            ) -> Result<(), $crate::state::StateError> {
                $( $crate::state::State::load(&mut self.$($field).+, r)?; )*
                $(
                    let $this = &*self;
                    $crate::state::check($valid)?;
                )?
                Ok(())
            }
        }
    };
}
pub(crate) use persist;

#[cfg(test)]
mod tests {
    use super::*;

    struct Fields {
        gain: f32,
        counters: [i32; 3],
        blocks: usize,
        flag: bool,
        peak: Option<usize>,
        partitions: Vec<FftData>,
        nested: Nested,
    }

    struct Nested {
        seed: u32,
        spectrum: Option<[f32; 2]>,
    }

    persist!(Fields { gain, counters, blocks, flag, peak, partitions, nested.seed, nested.spectrum }
        valid: |fields| fields.blocks < 1000);

    const SIZES: Sizes = [40, 13, 12, 12];

    fn fields() -> Fields {
        let mut partition = FftData::ZERO;
        partition.re[3] = -2.5;
        partition.im[64] = 1.0e-30;
        Fields {
            gain: 0.25,
            counters: [-1, 0, i32::MAX],
            blocks: 999,
            flag: true,
            peak: Some(17),
            partitions: vec![FftData::ZERO, partition],
            nested: Nested {
                seed: 0x8000_0001,
                spectrum: Some([1.5, -0.0]),
            },
        }
    }

    fn blank() -> Fields {
        Fields {
            gain: 0.0,
            counters: [0; 3],
            blocks: 0,
            flag: false,
            peak: None,
            partitions: vec![FftData::ZERO; 2],
            nested: Nested {
                seed: 0,
                spectrum: None,
            },
        }
    }

    fn saved(fields: &Fields) -> Vec<u8> {
        let mut w = Writer::new(SIZES);
        fields.save(&mut w);
        w.finish()
    }

    fn loaded(bytes: &[u8]) -> Result<Fields, StateError> {
        let mut fields = blank();
        let mut r = Reader::new(bytes, SIZES)?;
        fields.load(&mut r)?;
        r.finish()?;
        Ok(fields)
    }

    #[test]
    fn every_kind_of_field_comes_back_bit_for_bit() {
        let original = fields();
        let bytes = saved(&original);
        let copy = loaded(&bytes).unwrap();
        assert_eq!(copy.gain, 0.25);
        assert_eq!(copy.counters, [-1, 0, i32::MAX]);
        assert_eq!((copy.blocks, copy.flag, copy.peak), (999, true, Some(17)));
        assert_eq!(copy.partitions[1].re[3], -2.5);
        assert_eq!(copy.partitions[1].im[64], 1.0e-30);
        assert_eq!(copy.nested.seed, 0x8000_0001);
        let spectrum = copy.nested.spectrum.unwrap();
        assert_eq!(spectrum.map(f32::to_bits), [1.5f32, -0.0].map(f32::to_bits));
        assert_eq!(saved(&copy), bytes);

        // An absent value takes one byte and comes back absent.
        let mut absent = fields();
        absent.peak = None;
        absent.nested.spectrum = None;
        let shorter = saved(&absent);
        assert_eq!(shorter.len(), bytes.len() - 8 - 8);
        let copy = loaded(&shorter).unwrap();
        assert_eq!((copy.peak, copy.nested.spectrum), (None, None));
    }

    #[test]
    fn the_header_is_checked_before_any_field() {
        let bytes = saved(&fields());
        assert_eq!(loaded(&[]).err(), Some(StateError::NotAState));
        assert_eq!(loaded(b"RIFF....WAVE").err(), Some(StateError::NotAState));
        assert_eq!(loaded(&bytes[..12]).err(), Some(StateError::Truncated));
        for cut in [HEADER, HEADER + CHECKSUM, bytes.len() - 1] {
            assert_eq!(loaded(&bytes[..cut]).err(), Some(StateError::Truncated));
        }

        let mut newer = bytes.clone();
        newer[8] = 2;
        assert_eq!(loaded(&newer).err(), Some(StateError::Version(2)));

        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(loaded(&longer).err(), Some(StateError::Corrupt));

        let other = Reader::new(&bytes, [40, 13, 12, 13]).err();
        assert_eq!(other, Some(StateError::Config));
        let other = Reader::new(&bytes, [13, 13, 12, 12]).err();
        assert_eq!(other, Some(StateError::Config));
    }

    #[test]
    fn a_flipped_bit_anywhere_is_caught() {
        let bytes = saved(&fields());
        for at in 0..bytes.len() {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0x10;
            assert!(loaded(&flipped).is_err(), "byte {at}");
        }
    }

    /// States that carry a matching checksum and still hold a value no canceller does.
    #[test]
    fn values_no_canceller_holds_are_refused() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut fields = fields();
            fields.gain = bad;
            assert_eq!(loaded(&saved(&fields)).err(), Some(StateError::Corrupt));
        }
        let mut fields = fields();
        fields.blocks = 1000;
        assert_eq!(loaded(&saved(&fields)).err(), Some(StateError::Corrupt));

        // A flag that is neither 0 nor 1, with the checksum made to match.
        let mut bytes = saved(&self::fields());
        let flag = HEADER + 4 + 3 * 4 + 8;
        assert_eq!(bytes[flag], 1);
        bytes[flag] = 2;
        let end = bytes.len() - CHECKSUM;
        let checksum = fnv1a(&bytes[..end]);
        bytes[end..].copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(loaded(&bytes).err(), Some(StateError::Corrupt));

        // Fewer fields than the reader wants, and more.
        let mut w = Writer::new(SIZES);
        1.0f32.save(&mut w);
        assert_eq!(loaded(&w.finish()).err(), Some(StateError::Corrupt));
        let mut w = Writer::new(SIZES);
        self::fields().save(&mut w);
        7u32.save(&mut w);
        assert_eq!(loaded(&w.finish()).err(), Some(StateError::Corrupt));
    }

    #[test]
    fn a_resizable_vector_keeps_its_length_within_its_capacity() {
        let mut w = Writer::new(SIZES);
        save_resizable(&[1.0, 2.0, 3.0, 4.0], &mut w);
        let bytes = w.finish();
        let load = |capacity: usize, unit: usize| {
            let mut values = Vec::with_capacity(capacity);
            values.resize(capacity, 9.0);
            let mut r = Reader::new(&bytes, SIZES)?;
            load_resizable(&mut values, unit, &mut r)?;
            r.finish()?;
            Ok::<_, StateError>(values)
        };
        assert_eq!(load(6, 2).unwrap(), [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(load(4, 4).unwrap(), [1.0, 2.0, 3.0, 4.0]);
        // Longer than what the canceller allocated, and not a whole number of blocks.
        assert_eq!(load(3, 1).err(), Some(StateError::Corrupt));
        assert_eq!(load(6, 3).err(), Some(StateError::Corrupt));
    }
}
