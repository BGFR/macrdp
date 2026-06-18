//! Request/response messages are nested structs with fields, encoded as NDR (network data
//! representation).
//!
//! Fixed-size fields are encoded in-line as they appear in the struct.
//!
//! Variable-sized fields (strings, byte arrays, sometimes structs) are encoded as pointers:
//! - in place of the field in the struct, a "pointer" is written
//! - the pointer value is 0x0002xxxx, where xxxx is an "index" in increments of 4
//! - for example, first pointer is 0x0002_0000, second is 0x0002_0004, third is 0x0002_0008 etc.
//! - the actual values are then appended at the end of the message, in the same order as their
//!   pointers appeared
//! - in the code below, "*_ptr" is the pointer value and "*_value" the actual data
//! - note that some fields (like arrays) will have a length prefix before the pointer and also
//!   before the actual data at the end of the message
//!
//! To deal with this, fixed-size structs only have encode/decode methods, while variable-size ones
//! have encode_ptr/decode_ptr and encode_value/decode_value methods. Messages are parsed linearly,
//! so decode_ptr/decode_value are called at different stages (same for encoding).
//!
//! Most of the above was reverse-engineered from FreeRDP: [smartcard_pack.c]
//!
//! [smartcard_pack.c]: https://github.com/FreeRDP/FreeRDP/blob/ff303a9bda911c54ffc1b9f2471acd79c897b075/libfreerdp/utils/smartcard_pack.c

use ironrdp_core::{DecodeResult, EncodeResult, ReadCursor, WriteCursor, ensure_size, invalid_field_err};
use ironrdp_pdu::utils::{self, CharacterSet};

pub trait Decode {
    fn decode_ptr(src: &mut ReadCursor<'_>, index: &mut u32) -> DecodeResult<Self>
    where
        Self: Sized;
    fn decode_value(&mut self, src: &mut ReadCursor<'_>, charset: Option<CharacterSet>) -> DecodeResult<()>;
}

pub trait Encode {
    fn encode_ptr(&self, index: &mut u32, dst: &mut WriteCursor<'_>) -> EncodeResult<()>;
    fn encode_value(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()>;
    fn size_ptr(&self) -> usize;
    fn size_value(&self) -> usize;
    fn size(&self) -> usize {
        self.size_ptr() + self.size_value()
    }
}

pub fn encode_ptr(length: Option<u32>, index: &mut u32, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
    ensure_size!(ctx: "encode_ptr", in: dst, size: ptr_size(length.is_some()));
    if let Some(length) = length {
        dst.write_u32(length);
    }

    dst.write_u32(0x0002_0000 + *index * 4);
    *index += 1;
    Ok(())
}

pub fn decode_ptr(src: &mut ReadCursor<'_>, index: &mut u32) -> DecodeResult<u32> {
    ensure_size!(ctx: "decode_ptr", in: src, size: size_of::<u32>());
    let ptr = src.read_u32();
    if ptr == 0 {
        // NULL pointer is OK. Don't update index.
        return Ok(ptr);
    }
    let expect_ptr = 0x0002_0000 + *index * 4;
    *index += 1;
    if ptr != expect_ptr {
        Err(invalid_field_err!("decode_ptr", "ptr", "ptr != expect_ptr"))
    } else {
        Ok(ptr)
    }
}

pub fn ptr_size(with_length: bool) -> usize {
    if with_length {
        size_of::<u32>() * 2
    } else {
        size_of::<u32>()
    }
}

/// Server-direction mirror of [`read_string_from_cursor`]: writes an NDR conformant+varying string —
/// `MaximumCount`, `Offset` (0), `ActualCount`, then the NUL-terminated string — padded with zeros to a
/// 4-byte boundary.
///
/// `MaximumCount`/`ActualCount` are the element count *including* the NUL terminator (UTF-16 code units for
/// [`CharacterSet::Unicode`], bytes for [`CharacterSet::Ansi`]). The trailing pad mirrors the read side,
/// which aligns on the absolute cursor position; every MS-RDPESC string field starts 4-byte aligned, so
/// padding the region to a multiple of 4 is equivalent and lets [`string_size`] predict it without a position.
pub fn write_string_to_cursor(dst: &mut WriteCursor<'_>, value: &str, charset: CharacterSet) -> EncodeResult<()> {
    const ALIGNMENT: usize = 4;
    let char_size = char_size(charset);
    let encoded_len = utils::encoded_str_len(value, charset, true); // bytes incl NUL
    let char_count = (encoded_len / char_size) as u32;

    ensure_size!(ctx: "ndr::write_string_to_cursor", in: dst, size: size_of::<u32>() * 3 + encoded_len);
    dst.write_u32(char_count); // MaximumCount
    dst.write_u32(0); // Offset
    dst.write_u32(char_count); // ActualCount
    utils::write_string_to_cursor(dst, value, charset, true)?;

    let pad = (ALIGNMENT - (dst.pos() % ALIGNMENT)) % ALIGNMENT;
    if pad > 0 {
        ensure_size!(ctx: "ndr::write_string_to_cursor pad", in: dst, size: pad);
        dst.write_slice(&[0u8; ALIGNMENT][..pad]);
    }
    Ok(())
}

/// The number of bytes [`write_string_to_cursor`] writes for `value` (the three NDR length fields + the
/// NUL-terminated string + 4-byte-alignment padding).
pub fn string_size(value: &str, charset: CharacterSet) -> usize {
    let region = size_of::<u32>() * 3 + utils::encoded_str_len(value, charset, true);
    region.next_multiple_of(4)
}

fn char_size(charset: CharacterSet) -> usize {
    match charset {
        CharacterSet::Unicode => 2,
        CharacterSet::Ansi => 1,
    }
}

/// A special read_string_from_cursor which reads and ignores the additional length and
/// offset fields prefixing the string, as well as any extra padding for a 4-byte aligned
/// NULL-terminated string.
pub fn read_string_from_cursor(cursor: &mut ReadCursor<'_>, charset: CharacterSet) -> DecodeResult<String> {
    const ALIGNMENT: usize = 4;
    ensure_size!(ctx: "ndr::read_string_from_cursor", in: cursor, size: size_of::<u32>() * 3);
    let _length = cursor.read_u32();
    let _offset = cursor.read_u32();
    let _length2 = cursor.read_u32();

    let string = utils::read_string_from_cursor(cursor, charset, true)?;

    // Skip padding for 4-byte aligned NULL-terminated string.
    let mut pad = cursor.pos();
    let size = (pad + ALIGNMENT - 1) & !(ALIGNMENT - 1);
    pad = size - pad;
    if pad > 0 {
        ensure_size!(ctx: "ndr::read_string_from_cursor", in: cursor, size: pad);
        cursor.advance(pad);
    }

    Ok(string)
}
