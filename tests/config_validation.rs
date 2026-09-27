#[test]
fn server_rejects_unvalidated_limits() {
    let mut config = dragon_server::config::Config::load(std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/minimal/dragon.toml"
    )))
    .unwrap();
    config.limits.max_connections = 0;
    assert!(dragon_server::server::Server::new(config).is_err());
}
use dragon_server::config::Config;
use std::path::Path;

#[test]
fn example_is_valid() {
    let config = Config::load(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples/minimal/dragon.toml")
            .as_path(),
    )
    .unwrap();
    assert_eq!(config.sites.len(), 1);
    assert_eq!(config.limits.max_connections, 1024);
}

#[test]
fn rejects_invalid_configuration() {
    let example = include_str!("../examples/minimal/dragon.toml");
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/minimal");
    for text in [
        example.replace("schema_version = 1", "schema_version = 2"),
        example.replace("level = \"info\"", "unknown = true"),
        example.replace("status = 200", "status = 204"),
        example.replace("Hello from Dragon", "Hello\"broken"),
        example.replace("methods = [\"GET\", \"HEAD\"]", "methods = []"),
        example.replace("index = \"index.txt\"", "index = \"../secret\""),
        format!(
            "{example}\n[[sites.routes]]\npath='/hello'\nmatch='exact'\nmethods=['GET']\naction='respond'\nstatus=200\ncontent_type='text/plain'\nbody='duplicate'"
        ),
    ] {
        match toml::from_str::<Config>(&text) {
            Ok(mut config) => assert!(config.validate(&base).is_err()),
            Err(_) => continue,
        }
    }
}
