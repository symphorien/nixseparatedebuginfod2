use std::io::Read;
use std::path::Path;
use std::sync::Once;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

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

pub fn setup_logging() {
    SETUP_LOGGING.call_once(|| {
        let fmt_layer = tracing_subscriber::fmt::layer().with_test_writer();
        tracing_subscriber::registry().with(fmt_layer).init();
    });
}
