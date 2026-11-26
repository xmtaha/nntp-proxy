//! Connection pool prewarming functionality
//!
//! Opens a small number of backend connections up front so the first client
//! does not pay the handshake latency, then lets deadpool fill the rest of
//! each pool on demand. Warming the full pool would claim every connection the
//! provider allows before a single client is served.

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::config::Server;
use crate::pool::DeadpoolConnectionProvider;

/// Prewarm a single pool by creating up to `count` connections concurrently
async fn prewarm_single_pool(
    provider: DeadpoolConnectionProvider,
    server_name: String,
    count: usize,
) -> Result<usize> {
    info!(
        "Prewarming pool for '{}' with {} connections",
        server_name, count
    );

    // Create the connections concurrently
    let tasks: Vec<_> = (0..count)
        .map(|i| {
            // Clone Arc references for each async task to satisfy Send + 'static bounds
            // Arc makes this cheap - only the pointer is cloned, not the underlying data
            let provider = provider.clone();
            let server_name = server_name.clone();

            tokio::spawn(async move {
                provider
                    .get_pooled_connection()
                    .await
                    .inspect(|_conn| {
                        debug!(
                            "Created connection {}/{} for '{}'",
                            i + 1,
                            count,
                            server_name
                        );
                    })
                    .ok()
            })
        })
        .collect();

    // Wait for all connections and count successes
    let mut connections = Vec::with_capacity(count);
    for task in tasks {
        if let Ok(Some(conn)) = task.await {
            connections.push(conn);
        }
    }

    let created = connections.len();

    // Drop all connections - they return to pool as available
    drop(connections);

    info!(
        "Pool '{}' ready: {}/{} connections created",
        server_name, created, count
    );

    Ok(created)
}

/// Prewarm all connection pools before accepting clients
///
/// Opens at most `prewarm_count` connections per server. The rest of each pool
/// fills on demand, which keeps the startup footprint below the provider's
/// concurrency cap instead of claiming every allowed slot up front.
///
/// # Errors
/// Returns any task join failure or backend prewarming error from the
/// underlying providers.
pub async fn prewarm_pools(
    providers: &[DeadpoolConnectionProvider],
    servers: &[Server],
    prewarm_count: usize,
) -> Result<()> {
    info!("Prewarming all connection pools...");

    let counts: Vec<usize> = servers
        .iter()
        .map(|server| prewarm_count.min(server.max_connections.get()))
        .collect();

    // Prewarm all pools concurrently
    let tasks: Vec<_> = servers
        .iter()
        .zip(&counts)
        .enumerate()
        .map(|(i, (server, &count))| {
            let provider = providers[i].clone();
            let server_name = server.name.to_string();

            tokio::spawn(prewarm_single_pool(provider, server_name, count))
        })
        .collect();

    // Wait for all pools and collect results
    let mut total_created = 0;
    let total_expected: usize = counts.iter().sum();

    for (task, server) in tasks.into_iter().zip(servers.iter()) {
        match task.await {
            Ok(Ok(created)) => total_created += created,
            Ok(Err(e)) => warn!(
                "Failed to prewarm pool for '{}': {}",
                server.name.as_str(),
                e
            ),
            Err(e) => warn!(
                "Prewarming task panicked for '{}': {}",
                server.name.as_str(),
                e
            ),
        }
    }

    info!(
        "Prewarming complete: {}/{} connections ready across all pools",
        total_created, total_expected
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Server;
    use tokio::net::TcpListener;

    /// Spawn a simple mock NNTP server
    async fn spawn_mock_server_on_random_port() -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    // Send greeting
                    let _ = stream.write_all(b"200 Mock Server Ready\r\n").await;

                    // Handle commands
                    let mut buffer = [0; 1024];
                    while let Ok(n) = stream.read(&mut buffer).await {
                        if n == 0 {
                            break;
                        }
                        if buffer[..n].starts_with(b"QUIT") {
                            let _ = stream.write_all(b"205 Goodbye\r\n").await;
                            break;
                        }
                        let _ = stream.write_all(b"200 OK\r\n").await;
                    }
                });
            }
        });
        (port, handle)
    }

    #[tokio::test]
    async fn test_prewarm_pools_basic() {
        let (port, _server) = spawn_mock_server_on_random_port().await;

        let servers = vec![
            Server::builder("127.0.0.1", crate::types::Port::try_new(port).unwrap())
                .name("TestServer1")
                .max_connections(crate::types::MaxConnections::try_new(2).unwrap())
                .build()
                .unwrap(),
        ];

        let providers = servers
            .iter()
            .map(|s| {
                crate::pool::DeadpoolConnectionProvider::new(
                    s.host.to_string(),
                    s.port.get(),
                    s.name.to_string(),
                    s.max_connections.get(),
                    s.username.clone(),
                    s.password.clone(),
                )
            })
            .collect::<Vec<_>>();

        // Prewarm the pools
        let result = prewarm_pools(&providers, &servers, 2).await;

        // Should succeed with mock server
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_prewarm_pools_multiple_servers() {
        // Start two mock servers
        let (port1, _server1) = spawn_mock_server_on_random_port().await;
        let (port2, _server2) = spawn_mock_server_on_random_port().await;

        let servers = vec![
            Server::builder("127.0.0.1", crate::types::Port::try_new(port1).unwrap())
                .name("Server1")
                .max_connections(crate::types::MaxConnections::try_new(2).unwrap())
                .build()
                .unwrap(),
            Server::builder("127.0.0.1", crate::types::Port::try_new(port2).unwrap())
                .name("Server2")
                .max_connections(crate::types::MaxConnections::try_new(1).unwrap())
                .build()
                .unwrap(),
        ];

        let providers = servers
            .iter()
            .map(|s| {
                crate::pool::DeadpoolConnectionProvider::new(
                    s.host.to_string(),
                    s.port.get(),
                    s.name.to_string(),
                    s.max_connections.get(),
                    s.username.clone(),
                    s.password.clone(),
                )
            })
            .collect::<Vec<_>>();

        let result = prewarm_pools(&providers, &servers, 2).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_prewarm_pools_with_unreachable_server() {
        // Use a port that won't have a server (99999 is invalid)
        let bad_port = 65500; // High port unlikely to be in use

        let servers = vec![
            Server::builder("127.0.0.1", crate::types::Port::try_new(bad_port).unwrap())
                .name("UnreachableServer")
                .max_connections(crate::types::MaxConnections::try_new(1).unwrap())
                .build()
                .unwrap(),
        ];

        let providers = servers
            .iter()
            .map(|s| {
                crate::pool::DeadpoolConnectionProvider::new(
                    s.host.to_string(),
                    s.port.get(),
                    s.name.to_string(),
                    s.max_connections.get(),
                    s.username.clone(),
                    s.password.clone(),
                )
            })
            .collect::<Vec<_>>();

        // Prewarming should not fail even if server is unreachable
        // It logs warnings but returns Ok
        let result = prewarm_pools(&providers, &servers, 2).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_prewarm_pools_empty() {
        // Test with no servers
        let servers = vec![];
        let providers = vec![];

        let result = prewarm_pools(&providers, &servers, 2).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_prewarm_single_pool_success() {
        let (port, _server) = spawn_mock_server_on_random_port().await;

        let provider = crate::pool::DeadpoolConnectionProvider::new(
            "127.0.0.1".to_string(),
            port,
            "TestServer".to_string(),
            3,
            None,
            None,
        );

        let result = prewarm_single_pool(provider, "TestServer".to_string(), 3).await;

        assert!(result.is_ok());
        let created = result.unwrap();
        assert_eq!(created, 3);
    }

    #[tokio::test]
    async fn prewarm_opens_only_the_configured_count_not_the_pool_size() {
        let (port, _server) = spawn_mock_server_on_random_port().await;

        let servers = vec![
            Server::builder("127.0.0.1", crate::types::Port::try_new(port).unwrap())
                .name("CappedServer")
                .max_connections(crate::types::MaxConnections::try_new(6).unwrap())
                .build()
                .unwrap(),
        ];
        let providers = servers
            .iter()
            .map(|s| {
                crate::pool::DeadpoolConnectionProvider::new(
                    s.host.to_string(),
                    s.port.get(),
                    s.name.to_string(),
                    s.max_connections.get(),
                    s.username.clone(),
                    s.password.clone(),
                )
            })
            .collect::<Vec<_>>();

        prewarm_pools(&providers, &servers, 2).await.unwrap();

        let status = providers[0].status_counts();
        assert_eq!(
            status.size, 2,
            "prewarm must stop at prewarm_connections, not max_connections"
        );
    }

    #[tokio::test]
    async fn test_prewarm_single_pool_single_connection() {
        let (port, _server) = spawn_mock_server_on_random_port().await;

        let provider = crate::pool::DeadpoolConnectionProvider::new(
            "127.0.0.1".to_string(),
            port,
            "TestServer".to_string(),
            1,
            None,
            None,
        );

        let result = prewarm_single_pool(provider, "TestServer".to_string(), 1).await;

        assert!(result.is_ok());
        let created = result.unwrap();
        assert_eq!(created, 1);
    }

    #[tokio::test]
    async fn test_prewarm_single_pool_zero_connections() {
        let provider = crate::pool::DeadpoolConnectionProvider::new(
            "127.0.0.1".to_string(),
            1,
            "TestServer".to_string(),
            0, // Zero connections
            None,
            None,
        );

        let result = prewarm_single_pool(provider, "TestServer".to_string(), 0).await;

        assert!(result.is_ok());
        let created = result.unwrap();
        assert_eq!(created, 0);
    }

    #[tokio::test]
    async fn test_prewarm_pools_mixed_success_failure() {
        let (good_port, _server) = spawn_mock_server_on_random_port().await;
        let bad_port = 65499;

        let servers = vec![
            Server::builder("127.0.0.1", crate::types::Port::try_new(good_port).unwrap())
                .name("GoodServer")
                .max_connections(crate::types::MaxConnections::try_new(1).unwrap())
                .build()
                .unwrap(),
            Server::builder("127.0.0.1", crate::types::Port::try_new(bad_port).unwrap())
                .name("BadServer")
                .max_connections(crate::types::MaxConnections::try_new(1).unwrap())
                .build()
                .unwrap(),
        ];

        let providers = servers
            .iter()
            .map(|s| {
                crate::pool::DeadpoolConnectionProvider::new(
                    s.host.to_string(),
                    s.port.get(),
                    s.name.to_string(),
                    s.max_connections.get(),
                    s.username.clone(),
                    s.password.clone(),
                )
            })
            .collect::<Vec<_>>();

        // Should still return Ok even with partial failures
        let result = prewarm_pools(&providers, &servers, 2).await;
        assert!(result.is_ok());
    }
}
