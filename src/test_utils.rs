///! Functions used in tests only
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Once;
use tracing::Level;
use tracing_subscriber::filter;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// Returns the sha256sum of this file in a lowecase hex string
pub fn file_sha256(path: &Path) -> String {
    let mut file = std::fs::File::open(path).unwrap();
    let mut buf = [0; 4096];
    let mut hash = hmac_sha256::Hash::new();
    loop {
        let n = file.read(&mut buf).unwrap();
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
