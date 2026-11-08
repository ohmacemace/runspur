use serde::{Serialize, de::DeserializeOwned};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub async fn write_frame<W: AsyncWrite + Unpin + ?Sized, T: Serialize>(
    writer: &mut W,
    value: &T,
    max: usize,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Frame exceeds the configured limit",
        ));
    }
    writer.write_u32_le(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

pub async fn read_frame<R: AsyncRead + Unpin + ?Sized, T: DeserializeOwned>(
    reader: &mut R,
    max: usize,
) -> io::Result<T> {
    let size = reader.read_u32_le().await? as usize;
    if size == 0 || size > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid or oversized frame",
        ));
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn bounded_frames_work_across_fragmented_streams() {
        let (mut a, mut b) = tokio::io::duplex(8);
        let write = tokio::spawn(async move {
            write_frame(&mut a, &serde_json::json!({"value":"中文 空格"}), 128)
                .await
                .unwrap();
        });
        let got: serde_json::Value = read_frame(&mut b, 128).await.unwrap();
        assert_eq!(got["value"], "中文 空格");
        write.await.unwrap();
        let (mut a, mut b) = tokio::io::duplex(8);
        a.write_u32_le(1025).await.unwrap();
        assert!(
            read_frame::<_, serde_json::Value>(&mut b, 1024)
                .await
                .is_err()
        );
    }
}
