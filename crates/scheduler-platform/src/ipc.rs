#[cfg(not(windows))]
use anyhow::bail;
use anyhow::{Context, Result};
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite};

use scheduler_core::ProcessIdentity;

pub trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}
pub type BoxStream = Pin<Box<dyn IoStream>>;

pub struct PeerConnection {
    pub stream: BoxStream,
    pub process: ProcessIdentity,
}

#[cfg(windows)]
pub struct LocalListener {
    endpoint: String,
    pending: tokio::net::windows::named_pipe::NamedPipeServer,
}

#[cfg(windows)]
impl LocalListener {
    pub fn bind(endpoint: &str) -> Result<Self> {
        Ok(Self {
            endpoint: endpoint.into(),
            pending: create_pipe(endpoint, true)?,
        })
    }
    pub async fn accept(&mut self) -> Result<PeerConnection> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
        self.pending.connect().await?;
        let mut pid = 0;
        if unsafe { GetNamedPipeClientProcessId(self.pending.as_raw_handle(), &mut pid) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let process = crate::process::identity(pid)?;
        let next = create_pipe(&self.endpoint, false)?;
        let connected = std::mem::replace(&mut self.pending, next);
        Ok(PeerConnection {
            stream: Box::pin(connected),
            process,
        })
    }
}

#[cfg(windows)]
fn create_pipe(
    endpoint: &str,
    first: bool,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let descriptor = crate::windows::SecurityDescriptor::user_only(false)?;
    let mut attributes = descriptor.attributes();
    let server = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                endpoint,
                (&mut attributes as *mut windows_sys::Win32::Security::SECURITY_ATTRIBUTES).cast(),
            )
    }?;
    Ok(server)
}

#[cfg(windows)]
pub async fn connect_pipe(endpoint: &str) -> Result<PeerConnection> {
    use std::os::windows::io::AsRawHandle;
    use tokio::net::windows::named_pipe::ClientOptions;
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    let stream = loop {
        match ClientOptions::new().open(endpoint) {
            Ok(stream) => break stream,
            Err(e) if e.raw_os_error() == Some(231) && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            Err(e) => return Err(e).context("Local pipe is unavailable"),
        }
    };
    let mut pid = 0;
    if unsafe { GetNamedPipeServerProcessId(stream.as_raw_handle(), &mut pid) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(PeerConnection {
        stream: Box::pin(stream),
        process: crate::process::identity(pid)?,
    })
}

#[cfg(not(windows))]
pub struct LocalListener;
#[cfg(not(windows))]
impl LocalListener {
    pub fn bind(_endpoint: &str) -> Result<Self> {
        bail!("Local server backend is not yet enabled on this release target")
    }
    pub async fn accept(&mut self) -> Result<PeerConnection> {
        bail!("Local server backend is not yet enabled on this release target")
    }
}
#[cfg(not(windows))]
pub async fn connect_pipe(_endpoint: &str) -> Result<PeerConnection> {
    bail!("Local IPC backend is not yet enabled on this release target")
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn current_user_pipe_has_peer_identity_and_a_single_owner() {
        let endpoint = format!(r"\\.\pipe\runspur-test-{}", uuid::Uuid::new_v4());
        let mut listener = LocalListener::bind(&endpoint).unwrap();
        assert!(LocalListener::bind(&endpoint).is_err());
        let server = tokio::spawn(async move {
            let mut peer = listener.accept().await.unwrap();
            assert_eq!(peer.process.pid, std::process::id());
            let value: serde_json::Value = crate::read_frame(&mut peer.stream, 1024).await.unwrap();
            crate::write_frame(&mut peer.stream, &value, 1024)
                .await
                .unwrap();
        });
        let mut client = connect_pipe(&endpoint).await.unwrap();
        assert_eq!(client.process.pid, std::process::id());
        crate::write_frame(
            &mut client.stream,
            &serde_json::json!({"marker":"ok"}),
            1024,
        )
        .await
        .unwrap();
        let got: serde_json::Value = crate::read_frame(&mut client.stream, 1024).await.unwrap();
        assert_eq!(got["marker"], "ok");
        server.await.unwrap();
    }
}
