use common::TIMEOUT;
use super_orchestrator::{
    docker::{Container, ContainerNetwork},
    sh, std_init, Result,
};

#[tokio::main]
async fn main() -> Result<()> {
    std_init()?;

    let dockerfile = "./dockerfiles/single_node.dockerfile";
    let container_target = "x86_64-unknown-linux-gnu";
    let logs_dir = "./logs";
    let entrypoint = "single_node_entrypoint";

    // build internal runner
    sh("cargo build --release --bin", &[
        entrypoint,
        "--target",
        container_target,
    ])
    .await?;

    let mut cn = ContainerNetwork::new(
        "test",
        vec![Container::new(
            "main",
            Some(dockerfile),
            None,
            &[],
            &[("./logs", "/logs")],
            &format!("./target/{container_target}/release/{entrypoint}"),
            &[],
        )],
        false,
        logs_dir,
    )?;
    cn.run_all(true).await?;
    cn.wait_with_timeout_all(true, TIMEOUT).await.unwrap();
    Ok(())
}
