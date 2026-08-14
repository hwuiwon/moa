//! Postgres container bootstrap for orchestrator service fixtures.

use super::*;
use sqlx::Connection as _;

pub(super) async fn start_postgres_container() -> Result<ContainerAsync<GenericImage>> {
    GenericImage::new(POSTGRES_IMAGE, POSTGRES_TAG)
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::seconds(1))
        .with_env_var("POSTGRES_DB", POSTGRES_DB)
        .with_env_var("POSTGRES_USER", POSTGRES_USER)
        .with_env_var("POSTGRES_PASSWORD", POSTGRES_PASSWORD)
        .with_cmd([
            "postgres",
            "-c",
            "shared_preload_libraries=pgaudit",
            "-c",
            "wal_level=logical",
            "-c",
            "max_replication_slots=10",
            "-c",
            "max_wal_senders=10",
        ])
        .start()
        .await
        .context("start Postgres testcontainer")
}

pub(super) async fn ensure_postgres_image(repo_root: &Path) -> Result<()> {
    let image = format!("{POSTGRES_IMAGE}:{POSTGRES_TAG}");
    let inspect_status = tokio::process::Command::new("docker")
        .args(["image", "inspect", &image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("inspect Postgres test image with Docker")?;
    if inspect_status.success() {
        return Ok(());
    }

    let build_status = tokio::process::Command::new("docker")
        .current_dir(repo_root)
        .args([
            "build",
            "-f",
            "docker/postgres/Dockerfile",
            "--build-arg",
            "PGVECTOR_REF=v0.8.2",
            "-t",
            &image,
            ".",
        ])
        .status()
        .await
        .context("build Postgres test image with Docker")?;
    if !build_status.success() {
        bail!("docker build for {image} failed with status {build_status}");
    }
    Ok(())
}

pub(super) async fn wait_for_postgres(postgres_url: &str) -> Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        let probe = tokio::time::timeout(
            Duration::from_secs(1),
            sqlx::PgConnection::connect(postgres_url),
        )
        .await;
        match probe {
            Ok(Ok(connection)) => {
                connection
                    .close()
                    .await
                    .context("close Postgres readiness connection")?;
                return Ok(());
            }
            Ok(Err(error)) if Instant::now() < deadline => {
                tracing::debug!(%error, "waiting for Postgres testcontainer");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Ok(Err(error)) => {
                return Err(error).context("Postgres testcontainer did not become ready");
            }
            Err(_) if Instant::now() < deadline => {
                tracing::debug!("Postgres testcontainer readiness probe timed out");
            }
            Err(error) => {
                return Err(error).context("Postgres testcontainer readiness probe timed out");
            }
        }
    }
}

/// Creates and verifies the dedicated login used by workspace maintenance.
pub async fn provision_workspace_maintenance_login(admin_url: &str) -> Result<String> {
    let suffix = Uuid::now_v7().simple().to_string();
    let login = format!("moa_test_workspace_maintenance_{}", &suffix[..20]);
    let password = Uuid::now_v7().simple().to_string();

    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(admin_url)
        .await
        .context("connect as fixture database owner to provision workspace maintenance login")?;
    let create_role: String = sqlx::query_scalar(
        "SELECT format(\
            'CREATE ROLE %I LOGIN NOINHERIT NOBYPASSRLS PASSWORD %L',\
            $1::text,\
            $2::text\
         )",
    )
    .bind(&login)
    .bind(&password)
    .fetch_one(&admin_pool)
    .await
    .context("quote fixture workspace maintenance login")?;
    sqlx::query(&create_role)
        .execute(&admin_pool)
        .await
        .context("create fixture workspace maintenance login")?;
    let grant_role: String =
        sqlx::query_scalar("SELECT format('GRANT moa_workspace_maintenance TO %I', $1::text)")
            .bind(&login)
            .fetch_one(&admin_pool)
            .await
            .context("quote fixture workspace maintenance role grant")?;
    sqlx::query(&grant_role)
        .execute(&admin_pool)
        .await
        .context("grant fixture workspace maintenance role")?;
    admin_pool.close().await;

    let mut maintenance_url =
        url::Url::parse(admin_url).context("parse fixture database URL for maintenance login")?;
    maintenance_url
        .set_username(&login)
        .map_err(|()| anyhow!("fixture database URL cannot carry a maintenance username"))?;
    maintenance_url
        .set_password(Some(&password))
        .map_err(|()| anyhow!("fixture database URL cannot carry a maintenance password"))?;
    let maintenance_url = maintenance_url.to_string();

    let maintenance_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&maintenance_url)
        .await
        .context("connect with fixture workspace maintenance login")?;
    let mut conn = maintenance_pool
        .acquire()
        .await
        .context("acquire fixture workspace maintenance connection")?;
    let (current_user, inherits, bypasses_rls, superuser, is_member): (
        String,
        bool,
        bool,
        bool,
        bool,
    ) = sqlx::query_as(
        r#"
        SELECT current_user::text,
               rol.rolinherit,
               rol.rolbypassrls,
               rol.rolsuper,
               pg_has_role(current_user, 'moa_workspace_maintenance', 'MEMBER')
        FROM pg_roles AS rol
        WHERE rol.rolname = current_user
        "#,
    )
    .fetch_one(&mut *conn)
    .await
    .context("verify fixture workspace maintenance login attributes")?;
    if current_user != login || inherits || bypasses_rls || superuser || !is_member {
        bail!(
            "fixture workspace maintenance login is not least privilege: current_user={current_user}, inherits={inherits}, bypasses_rls={bypasses_rls}, superuser={superuser}, is_member={is_member}"
        );
    }
    sqlx::query("SET ROLE moa_workspace_maintenance")
        .execute(&mut *conn)
        .await
        .context("enter fixture workspace maintenance role")?;
    let role_user: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(&mut *conn)
        .await
        .context("read fixture workspace maintenance role identity")?;
    if role_user != "moa_workspace_maintenance" {
        bail!("fixture maintenance SET ROLE selected unexpected user `{role_user}`");
    }
    sqlx::query("RESET ROLE")
        .execute(&mut *conn)
        .await
        .context("leave fixture workspace maintenance role")?;
    drop(conn);
    maintenance_pool.close().await;

    Ok(maintenance_url)
}
