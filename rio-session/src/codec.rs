use crate::{protocol::MAX_FRAME_SIZE, SessionError};
use bincode::{Decode, Encode};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::time::{Duration, Instant};

fn config() -> impl bincode::config::Config {
    bincode::config::standard().with_limit::<MAX_FRAME_SIZE>()
}

/// Check the encoded payload budget without allocating a serialization buffer.
pub fn encoded_size<T: Encode>(value: &T) -> Result<usize, SessionError> {
    let size = bincode::encode_into_std_write(value, &mut io::sink(), config())
        .map_err(|error| SessionError::codec(error.to_string()))?;
    if size > MAX_FRAME_SIZE {
        return Err(SessionError::protocol("frame exceeds maximum size"));
    }
    Ok(size)
}

pub fn encode<T: Encode>(value: &T) -> Result<Vec<u8>, SessionError> {
    let bytes = bincode::encode_to_vec(value, config())
        .map_err(|err| SessionError::codec(err.to_string()))?;
    if bytes.len() > MAX_FRAME_SIZE {
        return Err(SessionError::protocol("frame exceeds maximum size"));
    }
    Ok(bytes)
}

pub fn decode<T: Decode<()>>(bytes: &[u8]) -> Result<T, SessionError> {
    if bytes.len() > MAX_FRAME_SIZE {
        return Err(SessionError::protocol("frame exceeds maximum size"));
    }
    let (value, consumed) = bincode::decode_from_slice(bytes, config())
        .map_err(|err| SessionError::codec(err.to_string()))?;
    if consumed != bytes.len() {
        return Err(SessionError::protocol("trailing bytes in frame"));
    }
    Ok(value)
}

pub fn write_frame<W: Write, T: Encode>(
    writer: &mut W,
    value: &T,
) -> Result<(), SessionError> {
    let payload = encode(value)?;
    let length = u32::try_from(payload.len())
        .map_err(|_| SessionError::protocol("frame length overflows protocol"))?;
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<R: Read, T: Decode<()>>(reader: &mut R) -> Result<T, SessionError> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(SessionError::protocol("frame exceeds maximum size"));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    decode(&payload)
}

#[cfg(unix)]
pub fn write_frame_until<T: Encode>(
    writer: &mut std::os::unix::net::UnixStream,
    value: &T,
    deadline: Instant,
) -> Result<(), SessionError> {
    let payload = encode(value)?;
    let length = u32::try_from(payload.len())
        .map_err(|_| SessionError::protocol("frame length overflows protocol"))?;
    write_until(writer, &length.to_le_bytes(), deadline)?;
    write_until(writer, &payload, deadline)?;
    writer.flush()?;
    Ok(())
}

#[cfg(unix)]
pub fn read_frame_until<T: Decode<()>>(
    reader: &mut std::os::unix::net::UnixStream,
    deadline: Instant,
) -> Result<T, SessionError> {
    let mut length = [0; 4];
    read_until(reader, &mut length, deadline)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(SessionError::protocol("frame exceeds maximum size"));
    }
    let mut payload = vec![0; length];
    read_until(reader, &mut payload, deadline)?;
    if Instant::now() >= deadline {
        return Err(
            io::Error::new(io::ErrorKind::TimedOut, "frame deadline exceeded").into(),
        );
    }
    let value = decode(&payload)?;
    if Instant::now() >= deadline {
        return Err(
            io::Error::new(io::ErrorKind::TimedOut, "frame deadline exceeded").into(),
        );
    }
    Ok(value)
}

#[cfg(unix)]
fn write_until(
    writer: &mut std::os::unix::net::UnixStream,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), SessionError> {
    let mut written = 0;
    while written < bytes.len() {
        let timeout = remaining(deadline)?;
        writer.set_write_timeout(Some(timeout))?;
        match writer.write(&bytes[written..]) {
            Ok(0) => {
                return Err(
                    io::Error::new(io::ErrorKind::WriteZero, "socket closed").into()
                )
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_until(
    reader: &mut std::os::unix::net::UnixStream,
    bytes: &mut [u8],
    deadline: Instant,
) -> Result<(), SessionError> {
    let mut read = 0;
    while read < bytes.len() {
        let timeout = remaining(deadline)?;
        reader.set_read_timeout(Some(timeout))?;
        match reader.read(&mut bytes[read..]) {
            Ok(0) => {
                return Err(
                    io::Error::new(io::ErrorKind::UnexpectedEof, "socket closed").into(),
                )
            }
            Ok(count) => read += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn remaining(deadline: Instant) -> Result<Duration, SessionError> {
    let timeout = deadline.saturating_duration_since(Instant::now());
    if timeout.is_zero() {
        return Err(
            io::Error::new(io::ErrorKind::TimedOut, "frame deadline exceeded").into(),
        );
    }
    Ok(timeout)
}

pub fn is_timeout(error: &SessionError) -> bool {
    matches!(error, SessionError::Io(io_error) if matches!(io_error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_preflight_matches_encoding_and_enforces_payload_limit() {
        for length in [0, 250, 251, 65535, 65536] {
            let value = vec![0u8; length];
            assert_eq!(encoded_size(&value).unwrap(), encode(&value).unwrap().len());
        }
        // The collection prefix also counts against the payload budget.
        let value = vec![0u8; MAX_FRAME_SIZE];
        assert!(matches!(
            encoded_size(&value),
            Err(SessionError::Protocol(_))
        ));
    }
}
