//! Functions used in tests only

use reqwest::Url;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{LazyLock, Once};
use tracing::Level;
use tracing_subscriber::filter;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use crate::vfs::AsFile;

/// Returns the sha256sum of this file in a lowecase hex string
pub async fn file_sha256<F: AsFile>(file: F) -> String {
    let mut std_file = file.open().await.unwrap().into_std().await;
    let mut buf = [0; 4096];
    let mut hash = hmac_sha256::Hash::new();
    loop {
        let n = std_file.read(&mut buf).unwrap();
        if n == 0 {
            break;
        } else {
            hash.update(&buf[..n]);
        }
    }
    let digest = hash.finalize();
    let mut result = String::new();
    for &byte in digest.iter() {
        result.push_str(&format!("{:0>2x}", byte))
    }
    result
}

static SETUP_LOGGING: Once = Once::new();

/// Tests calling this function will have tracing log in a way compatible with cargo test.
pub fn setup_logging() {
    SETUP_LOGGING.call_once(|| {
        let filter = filter::Targets::new()
            .with_target("runtime", Level::DEBUG)
            .with_target("tokio", Level::DEBUG)
            .with_default(Level::TRACE);

        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_test_writer()
            .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
            .with_filter(filter);

        let registry = tracing_subscriber::registry().with(fmt_layer);

        #[cfg(feature = "tokio-console")]
        let registry = registry.with(console_subscriber::spawn());

        #[cfg(feature = "tracing-chrome")]
        let (chrome_layer, guard) = tracing_chrome::ChromeLayerBuilder::new().build();
        #[cfg(feature = "tracing-chrome")]
        let registry = registry.with(chrome_layer);
        #[cfg(feature = "tracing-chrome")]
        {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(30));
                eprintln!("writing chrome trace, visualize it at https://ui.perfetto.dev/");
                drop(guard);
            });
        }

        registry.init();
    });
}

/// Path to the `tests/fixture` folder of the repo.
pub fn fixture(path: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(path);
    assert!(path.exists());
    path
}

/// The url of a http binary cache serving `tests/fixtures/file_binary_cache`
///
/// Started on first access
pub static HTTP_BINARY_CACHE: LazyLock<Url> = LazyLock::new(start_http_binary_cache);

fn start_http_binary_cache() -> Url {
    let dir = fixture("file_binary_cache");
    let port = port_check::free_local_ipv4_port().unwrap();
    let server =
        http_handle::server::Server::new(&format!("127.0.0.1:{port}"), dir.to_str().unwrap());
    std::thread::spawn(move || server.start().unwrap());
    while !port_check::is_port_reachable_with_timeout(
        ("127.0.0.1", port),
        std::time::Duration::from_millis(300),
    ) {
        std::thread::sleep(std::time::Duration::from_millis(100))
    }
    Url::parse(&format!("http://127.0.0.1:{port}")).unwrap()
}
