use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .pretty()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    smol::block_on(async {
        let snapi_devices = scanners_and_such::scanner::snapi::Snapi::discover_devices(true)
            .await
            .unwrap();

        println!("{snapi_devices:#?}");
    });
}
