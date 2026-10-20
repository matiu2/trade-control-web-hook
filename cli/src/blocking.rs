//! Sync CLI bridges must keep the runtime alive while broker sockets are pooled.
use color_eyre::eyre::{Result, eyre};
use std::{future::Future, sync::OnceLock};
use tokio::runtime::Runtime;

pub fn run_blocking<F, T>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    static RUNTIME: OnceLock<std::result::Result<Runtime, std::io::Error>> = OnceLock::new();
    let runtime = RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
        })
        .as_ref()
        .map_err(|error| eyre!("starting shared CLI runtime: {error}"))?;
    runtime.block_on(future)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_and_its_server_survive_between_blocking_calls() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
        let (mut client, server) = run_blocking(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let client = tokio::net::TcpStream::connect(listener.local_addr()?).await?;
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await?;
                let mut request = [0_u8; 1];
                socket.read_exact(&mut request).await?;
                socket.write_all(&request).await?;
                color_eyre::Result::<()>::Ok(())
            });
            Ok((client, server))
        })
        .expect("first blocking read");
        run_blocking(async {
            client.write_all(b"x").await?;
            let mut response = [0_u8; 1];
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client.read_exact(&mut response),
            )
            .await??;
            server.await??;
            assert_eq!(&response, b"x");
            Ok(())
        })
        .expect("reuse socket after first call returns");
    }
}
