use std::{
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use deadpool_postgres::Pool;
use sha2::{Digest, Sha256};

pub const POSTGRES_IMAGE: &str =
    "postgres@sha256:d3e1620b530c944afa6e887d22eb899824da68e19c52024bf98f5220c88a65b2";
const DATABASE: &str = "payment_sdk";
const PASSWORD: &str = "payment-sdk-test";

const INITIALIZER_NAME: &str = "0001_init.sql";
const INITIALIZER: &str = include_str!("../../migrations/0001_init.sql");
const INITIALIZER_SHA256: &str = "4d45ff45eab2c718ab3eb554a818a11391fde4ca8806ff26be782d9f40676b7c";

const INSERT_REGISTRY_SENTINEL: &str = "\
INSERT INTO payment_wallets (
    id, chain, network, address, start_height, secret, created_at
) VALUES (
    'baseline-wallet', 'baseline-chain', 'baseline-network',
    'baseline-address', 42, decode('0011223344556677', 'hex'),
    TIMESTAMPTZ '2026-08-29 00:00:00+00'
)";

const REGISTRY_SENTINEL_UNCHANGED: &str = "\
SELECT COUNT(*) = 1 AND BOOL_AND(
    id = 'baseline-wallet'
    AND chain = 'baseline-chain'
    AND network = 'baseline-network'
    AND address = 'baseline-address'
    AND start_height = 42
    AND secret = decode('0011223344556677', 'hex')
    AND created_at = TIMESTAMPTZ '2026-08-29 00:00:00+00'
)
FROM payment_wallets";

static NEXT: AtomicU64 = AtomicU64::new(0);

pub struct TestDatabase {
    _container: OwnedContainer,
    port: u16,
    schema: String,
    pool: Pool,
}

struct OwnedContainer(String);

impl TestDatabase {
    pub async fn start() -> Self {
        verify_image();
        let identity = unique_identity();
        let container = format!("payment-sdk-postgres-{identity}");
        let schema = format!("test_{identity}").replace('-', "_");
        let output = docker(&[
            "run",
            "--detach",
            "--rm",
            "--name",
            &container,
            "--env",
            &format!("POSTGRES_PASSWORD={PASSWORD}"),
            "--env",
            &format!("POSTGRES_DB={DATABASE}"),
            "--publish",
            "127.0.0.1::5432",
            POSTGRES_IMAGE,
        ]);
        assert_success(&output, "start PostgreSQL test container");
        let container = OwnedContainer(container);

        let port = mapped_port(&container.0);
        let base_url = format!("postgres://postgres:{PASSWORD}@127.0.0.1:{port}/{DATABASE}");
        wait_until_ready(&container.0);
        let setup_pool = indexing_postgres::pool(&base_url, 2).expect("setup pool");
        let setup = setup_pool.get().await.expect("PostgreSQL test connection");
        let version: String = setup
            .query_one("SHOW server_version", &[])
            .await
            .expect("server version")
            .get(0);
        assert_eq!(
            version, "18.6",
            "tests require the reviewed PostgreSQL version"
        );
        setup
            .batch_execute(&format!(
                "CREATE SCHEMA {schema}; SET search_path TO {schema}"
            ))
            .await
            .expect("create isolated test schema");
        assert_eq!(
            checksum(INITIALIZER),
            INITIALIZER_SHA256,
            "schema initializer checksum changed: {INITIALIZER_NAME}"
        );
        setup
            .batch_execute(INITIALIZER)
            .await
            .unwrap_or_else(|error| {
                panic!("apply schema initializer {INITIALIZER_NAME} to owned schema: {error}")
            });
        setup
            .batch_execute(INSERT_REGISTRY_SENTINEL)
            .await
            .expect("insert registry preservation sentinel after initialization");
        let sentinel_unchanged: bool = setup
            .query_one(REGISTRY_SENTINEL_UNCHANGED, &[])
            .await
            .expect("read registry preservation sentinel")
            .get(0);
        assert!(
            sentinel_unchanged,
            "schema initialization changed the payment_wallets sentinel"
        );
        drop(setup);
        drop(setup_pool);

        let url = format!("{base_url}?options=-csearch_path%3D{schema}");
        let pool = indexing_postgres::pool(&url, 8).expect("isolated schema pool");
        let _connection = pool.get().await.expect("isolated schema connection");
        Self {
            _container: container,
            port,
            schema,
            pool,
        }
    }

    pub fn pool(&self) -> Pool {
        self.pool.clone()
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn pool_for_schema(&self, schema: &str) -> Pool {
        let url = format!(
            "{}?options=-csearch_path%3D{schema}",
            self.url_with_password(PASSWORD)
        );
        indexing_postgres::pool(&url, 2).expect("schema-specific test pool")
    }

    pub async fn registry_sentinel_unchanged(&self) -> bool {
        self.pool
            .get()
            .await
            .expect("registry sentinel connection")
            .query_one(REGISTRY_SENTINEL_UNCHANGED, &[])
            .await
            .expect("read registry preservation sentinel")
            .get(0)
    }

    pub fn url_with_password(&self, password: &str) -> String {
        format!(
            "postgres://postgres:{password}@127.0.0.1:{}/{DATABASE}",
            self.port
        )
    }
}

impl Drop for OwnedContainer {
    fn drop(&mut self) {
        let output = docker(&["rm", "--force", &self.0]);
        if !output.status.success() {
            eprintln!(
                "failed to remove owned PostgreSQL container {}: {}",
                self.0,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

fn verify_image() {
    let output = docker(&[
        "image",
        "inspect",
        "--format",
        "{{index .RepoDigests 0}}",
        POSTGRES_IMAGE,
    ]);
    assert_success(&output, "inspect PostgreSQL test image");
    let actual = String::from_utf8(output.stdout).expect("image digest is UTF-8");
    assert_eq!(
        actual.trim(),
        POSTGRES_IMAGE,
        "PostgreSQL image digest drifted"
    );
}

fn unique_identity() -> String {
    let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    format!("{}-{stamp}-{ordinal}", std::process::id())
}

fn mapped_port(container: &str) -> u16 {
    for _ in 0..100 {
        let output = docker(&["port", container, "5432/tcp"]);
        if output.status.success() {
            let text = String::from_utf8(output.stdout).expect("docker port output is UTF-8");
            if let Some(port) = text
                .trim()
                .rsplit(':')
                .next()
                .and_then(|value| value.parse().ok())
            {
                return port;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("owned PostgreSQL container did not publish its port");
}

fn wait_until_ready(container: &str) {
    let mut consecutive = 0;
    for _ in 0..200 {
        if docker(&[
            "exec", container, "psql", "-U", "postgres", "-d", DATABASE, "-Atc", "SELECT 1",
        ])
        .status
        .success()
        {
            consecutive += 1;
            if consecutive == 3 {
                return;
            }
        } else {
            consecutive = 0;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("owned PostgreSQL container did not become ready");
}

fn checksum(sql: &str) -> String {
    format!("{:x}", Sha256::digest(sql.as_bytes()))
}

fn docker(arguments: &[&str]) -> Output {
    Command::new("docker")
        .args(arguments)
        .output()
        .expect("Docker must be installed for PostgreSQL repository tests")
}

fn assert_success(output: &Output, action: &str) {
    assert!(
        output.status.success(),
        "could not {action}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
