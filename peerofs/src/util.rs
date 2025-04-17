use std::io::Read;

pub(crate) fn read_u8_array<const N: usize, R: Read>(reader: &mut R) -> std::io::Result<[u8; N]> {
    let mut buf = [0u8; N];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}
