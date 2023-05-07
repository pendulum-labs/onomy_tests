use common::ONOMY_BASE;
use super_orchestrator::{
    docker::{Container, ContainerNetwork},
    Command, Result,
};

#[tokio::main]
async fn main() -> Result<()> {
    let dockerfile = "./dockerfiles/chain_upgrade_test.dockerfile";
    let container_target = "x86_64-unknown-linux-gnu";
    let logs_dir = "./logs";
    let entrypoint = "chain_upgrade_test_entrypoint";

    // build internal runner
    Command::new("cargo", &[
        "build",
        "--release",
        "--bin",
        &entrypoint,
        "--target",
        &container_target,
    ])
    .ci_mode(true)
    .run_to_completion()
    .await?
    .assert_success()?;

    let mut cn = ContainerNetwork::new(
        "test",
        vec![Container::new(
            "main",
            ONOMY_BASE,
            &[],
            &format!("./target/{container_target}/release/{entrypoint}"),
            &[],
        )],
        false,
        logs_dir,
    );
    cn.run().await?;

    Ok(())
}
