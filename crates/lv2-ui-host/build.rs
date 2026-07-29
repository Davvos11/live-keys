fn main() {
    pkg_config::Config::new()
        .atleast_version("0.10")
        .probe("suil-0")
        .expect("suil-0 not found via pkg-config; install libsuil-dev");
}
