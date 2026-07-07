fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = &[
        "proto/common/envelope.proto",
        "proto/common/types/base_types.proto",
        "proto/common/types/enums.proto",
        "proto/common/types/project_settings.proto",
        "proto/common/commands/base_commands.proto",
        "proto/common/commands/editor_commands.proto",
        "proto/common/commands/project_commands.proto",
        "proto/board/board.proto",
        "proto/board/board_commands.proto",
        "proto/board/board_types.proto",
    ];

    // Include paths: our proto dir + protoc's well-known types
    // The PROTOC env var points to the protoc binary; its sibling ../include/ has google protos
    let protoc_path = std::env::var("PROTOC").unwrap_or_else(|_| "protoc".to_string());
    let protoc_dir = std::path::Path::new(&protoc_path)
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("include"))
        .unwrap_or_default();

    let mut includes: Vec<&str> = vec!["proto/"];
    let protoc_include = protoc_dir.to_str().unwrap_or("");
    if !protoc_include.is_empty() && std::path::Path::new(protoc_include).exists() {
        includes.push(protoc_include);
    }

    prost_build::Config::new().compile_protos(protos, &includes)?;

    Ok(())
}
