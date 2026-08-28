fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc is available");
    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc);
    config
        .compile_protos(&["../../proto/seeed/hal/v1/hal.proto"], &["../../proto"])
        .expect("HAL v1 protobuf contract compiles");
    println!("cargo:rerun-if-changed=../../proto/seeed/hal/v1/hal.proto");
}
