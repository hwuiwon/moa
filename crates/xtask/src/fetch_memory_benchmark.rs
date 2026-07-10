//! Pinned, atomic fetcher for external-memory benchmark packages.

use std::env;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use moa_eval::external_memory::dataset::{
    DatasetFileProvenance, DatasetPackageManifestV1, DatasetPackageSourceV1, DatasetPackageV1,
    LongMemEvalFetchSummaryV1, PersonaMemFetchSummaryV1, VerifiedFetchSummaryV1,
};
use moa_eval::external_memory::longmemeval::{
    LONGMEMEVAL_ABSTENTION_COUNT, LONGMEMEVAL_DATASET, LONGMEMEVAL_FILE, LONGMEMEVAL_FILE_SHA256,
    LONGMEMEVAL_FILE_SIZE_BYTES, LONGMEMEVAL_PACKAGE_SHA256, LONGMEMEVAL_QUESTION_COUNT,
    LONGMEMEVAL_REPOSITORY, LONGMEMEVAL_RETRIEVAL_COUNT, LONGMEMEVAL_REVISION,
    load_longmemeval_file,
};
use moa_eval::external_memory::personamem::{
    PERSONAMEM_CONTEXT_COUNT, PERSONAMEM_DATASET, PERSONAMEM_PACKAGE_SHA256,
    PERSONAMEM_PERSONA_COUNT, PERSONAMEM_QUESTION_COUNT, PERSONAMEM_QUESTIONS_FILE,
    PERSONAMEM_QUESTIONS_SHA256, PERSONAMEM_QUESTIONS_SIZE_BYTES, PERSONAMEM_REPOSITORY,
    PERSONAMEM_REVISION, PERSONAMEM_SHARED_CONTEXTS_FILE, PERSONAMEM_SHARED_CONTEXTS_SHA256,
    PERSONAMEM_SHARED_CONTEXTS_SIZE_BYTES, load_personamem_files,
};
use reqwest::redirect::Policy;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const NETWORK_FLAG: &str = "MOA_RUN_NETWORK_MEMORY_BENCHMARKS";
const QUESTIONS_URL: &str = "https://huggingface.co/datasets/bowen-upenn/PersonaMem-v1/resolve/73dfd752d477d0c466cd441f1669397f5726d7ab/questions_32k.csv?download=true";
const CONTEXTS_URL: &str = "https://huggingface.co/datasets/bowen-upenn/PersonaMem-v1/resolve/73dfd752d477d0c466cd441f1669397f5726d7ab/shared_contexts_32k.jsonl?download=true";
const LONGMEMEVAL_URL: &str = "https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/98d7416c24c778c2fee6e6f3006e7a073259d48f/longmemeval_s_cleaned.json?download=true";

#[derive(Debug, Clone)]
struct BenchmarkFileSpec {
    path: &'static str,
    url: &'static str,
    size_bytes: u64,
    sha256: String,
}

impl BenchmarkFileSpec {
    fn provenance(&self) -> DatasetFileProvenance {
        DatasetFileProvenance {
            path: self.path.to_string(),
            size_bytes: self.size_bytes,
            sha256: self.sha256.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct BenchmarkSpec {
    manifest: DatasetPackageManifestV1,
    package_sha256: String,
    files: Vec<BenchmarkFileSpec>,
    validator: BenchmarkValidator,
}

#[derive(Debug, Clone)]
enum BenchmarkValidator {
    PersonaMem32k {
        question_count: usize,
        persona_count: usize,
        context_count: usize,
    },
    LongMemEval {
        question_count: usize,
        abstention_count: usize,
        retrieval_count: usize,
    },
}

impl BenchmarkSpec {
    fn for_dataset(dataset: &str, revision: &str) -> Result<Self> {
        let spec = match dataset {
            PERSONAMEM_DATASET => Self::personamem_32k(),
            LONGMEMEVAL_DATASET => Self::longmemeval_s_cleaned(),
            _ => bail!("unsupported memory benchmark dataset: {dataset}"),
        };
        if revision != spec.manifest.source.revision {
            bail!(
                "{} requires pinned revision {}",
                spec.manifest.dataset,
                spec.manifest.source.revision
            );
        }
        Ok(spec)
    }

    fn personamem_32k() -> Self {
        let files = vec![
            BenchmarkFileSpec {
                path: PERSONAMEM_QUESTIONS_FILE,
                url: QUESTIONS_URL,
                size_bytes: PERSONAMEM_QUESTIONS_SIZE_BYTES,
                sha256: PERSONAMEM_QUESTIONS_SHA256.to_string(),
            },
            BenchmarkFileSpec {
                path: PERSONAMEM_SHARED_CONTEXTS_FILE,
                url: CONTEXTS_URL,
                size_bytes: PERSONAMEM_SHARED_CONTEXTS_SIZE_BYTES,
                sha256: PERSONAMEM_SHARED_CONTEXTS_SHA256.to_string(),
            },
        ];
        let manifest = DatasetPackageManifestV1 {
            schema_version: 1,
            dataset: PERSONAMEM_DATASET.to_string(),
            source: DatasetPackageSourceV1 {
                repository: PERSONAMEM_REPOSITORY.to_string(),
                revision: PERSONAMEM_REVISION.to_string(),
            },
            files: files.iter().map(BenchmarkFileSpec::provenance).collect(),
        };
        Self {
            manifest,
            package_sha256: PERSONAMEM_PACKAGE_SHA256.to_string(),
            files,
            validator: BenchmarkValidator::PersonaMem32k {
                question_count: PERSONAMEM_QUESTION_COUNT,
                persona_count: PERSONAMEM_PERSONA_COUNT,
                context_count: PERSONAMEM_CONTEXT_COUNT,
            },
        }
    }

    fn longmemeval_s_cleaned() -> Self {
        let files = vec![BenchmarkFileSpec {
            path: LONGMEMEVAL_FILE,
            url: LONGMEMEVAL_URL,
            size_bytes: LONGMEMEVAL_FILE_SIZE_BYTES,
            sha256: LONGMEMEVAL_FILE_SHA256.to_string(),
        }];
        let manifest = DatasetPackageManifestV1 {
            schema_version: 1,
            dataset: LONGMEMEVAL_DATASET.to_string(),
            source: DatasetPackageSourceV1 {
                repository: LONGMEMEVAL_REPOSITORY.to_string(),
                revision: LONGMEMEVAL_REVISION.to_string(),
            },
            files: files.iter().map(BenchmarkFileSpec::provenance).collect(),
        };
        Self {
            manifest,
            package_sha256: LONGMEMEVAL_PACKAGE_SHA256.to_string(),
            files,
            validator: BenchmarkValidator::LongMemEval {
                question_count: LONGMEMEVAL_QUESTION_COUNT,
                abstention_count: LONGMEMEVAL_ABSTENTION_COUNT,
                retrieval_count: LONGMEMEVAL_RETRIEVAL_COUNT,
            },
        }
    }
}

#[derive(Debug)]
struct DownloadResponse {
    status: u16,
    final_url: String,
    bytes: Vec<u8>,
}

#[async_trait]
trait MemoryBenchmarkTransport: Send + Sync {
    async fn get(&self, url: &str) -> Result<DownloadResponse>;
}

struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    fn new() -> Result<Self> {
        let redirect = Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.stop();
            }
            if let Err(error) = validate_download_url(attempt.url()) {
                return attempt.error(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    error.to_string(),
                ));
            }
            attempt.follow()
        });
        let client = reqwest::Client::builder()
            .redirect(redirect)
            .build()
            .context("build memory benchmark HTTP client")?;
        Ok(Self { client })
    }
}

#[async_trait]
impl MemoryBenchmarkTransport for ReqwestTransport {
    async fn get(&self, url: &str) -> Result<DownloadResponse> {
        let parsed = reqwest::Url::parse(url).context("parse fixed memory benchmark URL")?;
        validate_download_url(&parsed)?;
        let response = self
            .client
            .get(parsed)
            .send()
            .await
            .context("download memory benchmark file")?;
        let status = response.status().as_u16();
        let final_url = response.url().to_string();
        let bytes = response
            .bytes()
            .await
            .context("read memory benchmark response body")?
            .to_vec();
        Ok(DownloadResponse {
            status,
            final_url,
            bytes,
        })
    }
}

/// Parses and runs the separately authorized benchmark fetch command.
pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let args = parse_args(args)?;
    if env::var(NETWORK_FLAG).as_deref() != Ok("1") {
        bail!("network benchmark fetch requires {NETWORK_FLAG}=1");
    }
    let spec = BenchmarkSpec::for_dataset(&args.dataset, &args.revision)?;
    validate_target_path(&args.output)?;
    validate_target_path(&args.summary_output)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build memory benchmark fetch runtime")?;
    let summary = runtime.block_on(fetch_with_transport(
        &ReqwestTransport::new()?,
        &spec,
        &args.output,
        &args.summary_output,
    ))?;
    println!(
        "verified {} questions for {}: {}",
        summary.question_count(),
        args.dataset,
        args.output.display()
    );
    Ok(())
}

struct FetchArgs {
    dataset: String,
    revision: String,
    output: PathBuf,
    summary_output: PathBuf,
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<FetchArgs> {
    let mut values = std::collections::BTreeMap::new();
    let mut args = args;
    while let Some(flag) = args.next() {
        if !matches!(
            flag.as_str(),
            "--dataset" | "--revision" | "--output" | "--summary-output"
        ) {
            bail!("unknown fetch-memory-benchmark argument: {flag}");
        }
        let value = args
            .next()
            .with_context(|| format!("missing value for {flag}"))?;
        if values.insert(flag.clone(), value).is_some() {
            bail!("duplicate fetch-memory-benchmark argument: {flag}");
        }
    }
    let required = |flag: &str| {
        values
            .get(flag)
            .cloned()
            .with_context(|| format!("missing required argument {flag}"))
    };
    Ok(FetchArgs {
        dataset: required("--dataset")?,
        revision: required("--revision")?,
        output: required("--output")?.into(),
        summary_output: required("--summary-output")?.into(),
    })
}

async fn fetch_with_transport(
    transport: &dyn MemoryBenchmarkTransport,
    spec: &BenchmarkSpec,
    output: &Path,
    summary_output: &Path,
) -> Result<VerifiedFetchSummaryV1> {
    validate_target_path(output)?;
    validate_target_path(summary_output)?;
    if output.exists() {
        let summary = validate_published_package(spec, output)
            .context("existing benchmark destination is invalid; refusing to mutate it")?;
        write_summary_atomic(summary_output, &summary)?;
        return Ok(summary);
    }

    let parent = output
        .parent()
        .context("benchmark output must have a parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create benchmark output parent {}", parent.display()))?;
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .context("benchmark output must have a UTF-8 file name")?;
    let staging = parent.join(format!(".{name}.staging-{}", Uuid::new_v4()));
    std::fs::create_dir(&staging)
        .with_context(|| format!("create benchmark staging directory {}", staging.display()))?;
    let guard = StagingGuard(Some(staging.clone()));

    for file in &spec.files {
        let response = transport.get(file.url).await?;
        validate_response(file, &response)?;
        std::fs::write(staging.join(file.path), response.bytes)
            .with_context(|| format!("write staged benchmark file {}", file.path))?;
    }
    let package = DatasetPackageV1 {
        manifest: spec.manifest.clone(),
        package_sha256: spec.package_sha256.clone(),
    };
    package.validate().map_err(anyhow::Error::from)?;
    let package_bytes = serde_json::to_vec_pretty(&package).context("serialize package.json")?;
    std::fs::write(staging.join("package.json"), package_bytes)
        .context("write staged package.json")?;
    let summary = validate_published_package(spec, &staging)?;
    std::fs::rename(&staging, output).with_context(|| {
        format!(
            "atomically publish benchmark package {} -> {}",
            staging.display(),
            output.display()
        )
    })?;
    guard.disarm();
    write_summary_atomic(summary_output, &summary)?;
    Ok(summary)
}

fn validate_published_package(spec: &BenchmarkSpec, root: &Path) -> Result<VerifiedFetchSummaryV1> {
    let package_path = root.join("package.json");
    let package: DatasetPackageV1 = serde_json::from_slice(
        &std::fs::read(&package_path)
            .with_context(|| format!("read {}", package_path.display()))?,
    )
    .context("parse strict package.json")?;
    if package.manifest != spec.manifest || package.package_sha256 != spec.package_sha256 {
        bail!("published package provenance does not match the pinned benchmark spec");
    }
    package.verify_files(root).map_err(anyhow::Error::from)?;
    match &spec.validator {
        BenchmarkValidator::PersonaMem32k {
            question_count,
            persona_count,
            context_count,
        } => {
            let dataset = load_personamem_files(
                &root.join(PERSONAMEM_QUESTIONS_FILE),
                &root.join(PERSONAMEM_SHARED_CONTEXTS_FILE),
            )
            .map_err(anyhow::Error::from)?;
            if dataset.cases.len() != *question_count
                || dataset.persona_count() != *persona_count
                || dataset.context_count != *context_count
            {
                bail!(
                    "benchmark counts mismatch: expected {question_count} / {persona_count} / {context_count}, got {} / {} / {}",
                    dataset.cases.len(),
                    dataset.persona_count(),
                    dataset.context_count
                );
            }
            Ok(VerifiedFetchSummaryV1::PersonaMem(
                PersonaMemFetchSummaryV1 {
                    schema_version: 1,
                    dataset: spec.manifest.dataset.clone(),
                    repository: spec.manifest.source.repository.clone(),
                    revision: spec.manifest.source.revision.clone(),
                    package_sha256: spec.package_sha256.clone(),
                    question_count: dataset.cases.len(),
                    persona_count: dataset.persona_count(),
                    context_count: dataset.context_count,
                    verified: true,
                },
            ))
        }
        BenchmarkValidator::LongMemEval {
            question_count,
            abstention_count,
            retrieval_count,
        } => {
            let dataset =
                load_longmemeval_file(&root.join(LONGMEMEVAL_FILE)).map_err(anyhow::Error::from)?;
            if dataset.cases.len() != *question_count
                || dataset.abstention_count() != *abstention_count
                || dataset.retrieval_count() != *retrieval_count
            {
                bail!(
                    "benchmark counts mismatch: expected {question_count} / {abstention_count} / {retrieval_count}, got {} / {} / {}",
                    dataset.cases.len(),
                    dataset.abstention_count(),
                    dataset.retrieval_count()
                );
            }
            Ok(VerifiedFetchSummaryV1::LongMemEval(
                LongMemEvalFetchSummaryV1 {
                    schema_version: 1,
                    dataset: spec.manifest.dataset.clone(),
                    repository: spec.manifest.source.repository.clone(),
                    revision: spec.manifest.source.revision.clone(),
                    package_sha256: spec.package_sha256.clone(),
                    question_count: dataset.cases.len(),
                    abstention_count: dataset.abstention_count(),
                    retrieval_count: dataset.retrieval_count(),
                    verified: true,
                },
            ))
        }
    }
}

fn validate_response(file: &BenchmarkFileSpec, response: &DownloadResponse) -> Result<()> {
    if !(200..300).contains(&response.status) {
        bail!("download {} returned HTTP {}", file.path, response.status);
    }
    let final_url = reqwest::Url::parse(&response.final_url)
        .with_context(|| format!("parse final download URL for {}", file.path))?;
    validate_download_url(&final_url)?;
    let size_bytes =
        u64::try_from(response.bytes.len()).context("download size does not fit u64")?;
    if size_bytes != file.size_bytes {
        bail!(
            "download {} size mismatch: expected {}, got {size_bytes}",
            file.path,
            file.size_bytes
        );
    }
    let sha256 = format!("{:x}", Sha256::digest(&response.bytes));
    if sha256 != file.sha256 {
        bail!(
            "download {} SHA-256 mismatch: expected {}, got {sha256}",
            file.path,
            file.sha256
        );
    }
    Ok(())
}

fn validate_download_url(url: &reqwest::Url) -> Result<()> {
    if url.scheme() != "https" {
        bail!("memory benchmark redirects must remain HTTPS");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("memory benchmark URLs must not contain embedded credentials");
    }
    let host = url
        .host_str()
        .context("memory benchmark URL must have a host")?
        .to_ascii_lowercase();
    if host != "huggingface.co" && !host.ends_with(".hf.co") && !host.ends_with(".cloudfront.net") {
        bail!("memory benchmark redirect host is not allowlisted: {host}");
    }
    Ok(())
}

fn validate_target_path(path: &Path) -> Result<()> {
    let components = path.components().collect::<Vec<_>>();
    let target_position = components
        .iter()
        .position(|component| component.as_os_str() == "target");
    if components
        .iter()
        .any(|component| matches!(component, Component::ParentDir))
        || target_position.is_none_or(|position| position + 1 >= components.len())
        || path.file_name().is_none()
    {
        bail!(
            "benchmark outputs must be beneath target/: {}",
            path.display()
        );
    }
    Ok(())
}

fn write_summary_atomic(path: &Path, summary: &VerifiedFetchSummaryV1) -> Result<()> {
    let parent = path
        .parent()
        .context("benchmark summary must have a parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create summary parent {}", parent.display()))?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .context("summary file name must be UTF-8")?,
        Uuid::new_v4()
    ));
    let bytes = serde_json::to_vec_pretty(summary).context("serialize fetch summary")?;
    std::fs::write(&temporary, bytes)
        .with_context(|| format!("write temporary summary {}", temporary.display()))?;
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("publish fetch summary {}", path.display()));
    }
    Ok(())
}

struct StagingGuard(Option<PathBuf>);

impl StagingGuard {
    fn disarm(mut self) {
        self.0.take();
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use moa_eval::external_memory::dataset::{DatasetPackageManifestV1, DatasetPackageSourceV1};
    use moa_eval::external_memory::longmemeval::{
        LONGMEMEVAL_DATASET, LONGMEMEVAL_FILE, LONGMEMEVAL_REPOSITORY, LONGMEMEVAL_REVISION,
    };
    use moa_eval::external_memory::personamem::{
        PERSONAMEM_DATASET, PERSONAMEM_REPOSITORY, PERSONAMEM_REVISION,
    };
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    use super::{
        BenchmarkFileSpec, BenchmarkSpec, BenchmarkValidator, DownloadResponse,
        LongMemEvalFetchSummaryV1, MemoryBenchmarkTransport, PersonaMemFetchSummaryV1,
        fetch_with_transport,
    };
    use moa_eval::external_memory::dataset::VerifiedFetchSummaryV1;

    struct FakeTransport {
        responses: Mutex<BTreeMap<String, VecDeque<Result<DownloadResponse>>>>,
    }

    impl FakeTransport {
        fn new(responses: impl IntoIterator<Item = (String, Result<DownloadResponse>)>) -> Self {
            let mut queued = BTreeMap::<String, VecDeque<Result<DownloadResponse>>>::new();
            for (url, response) in responses {
                queued.entry(url).or_default().push_back(response);
            }
            Self {
                responses: Mutex::new(queued),
            }
        }

        fn no_requests() -> Self {
            Self::new([])
        }
    }

    #[async_trait]
    impl MemoryBenchmarkTransport for FakeTransport {
        async fn get(&self, url: &str) -> Result<DownloadResponse> {
            self.responses
                .lock()
                .expect("fake transport lock")
                .get_mut(url)
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(|| Err(anyhow!("unexpected request: {url}")))
        }
    }

    #[tokio::test]
    async fn fetch_memory_benchmark_partial_download_cleans_staging_and_publishes_nothing() {
        // Pins: a second-file transport failure leaves neither a package nor summary nor staging directory.
        let fixture = fixture();
        let spec = tiny_spec(&fixture);
        let transport = FakeTransport::new([
            (
                spec.files[0].url.to_string(),
                Ok(response(spec.files[0].url, fixture.questions.clone())),
            ),
            (
                spec.files[1].url.to_string(),
                Err(anyhow!("partial download")),
            ),
        ]);
        let temp = TempDir::new().expect("tempdir");
        let (output, summary) = target_paths(temp.path());
        assert!(
            fetch_with_transport(&transport, &spec, &output, &summary)
                .await
                .is_err()
        );
        assert!(!output.exists());
        assert!(!summary.exists());
        assert_no_staging(temp.path());
    }

    #[tokio::test]
    async fn fetch_memory_benchmark_hash_mismatch_preserves_existing_invalid_destination() {
        // Pins: invalid existing destinations fail without mutation and downloads never replace them.
        let fixture = fixture();
        let spec = tiny_spec(&fixture);
        let temp = TempDir::new().expect("tempdir");
        let (output, summary) = target_paths(temp.path());
        std::fs::create_dir_all(&output).expect("create invalid destination");
        let sentinel = output.join("sentinel.txt");
        std::fs::write(&sentinel, b"keep-me").expect("write sentinel");
        assert!(
            fetch_with_transport(&FakeTransport::no_requests(), &spec, &output, &summary)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&sentinel).expect("read sentinel"), b"keep-me");
        assert!(!summary.exists());

        std::fs::remove_dir_all(&output).expect("remove invalid destination");
        let bad_transport = FakeTransport::new([
            (
                spec.files[0].url.to_string(),
                Ok(response(spec.files[0].url, b"wrong".to_vec())),
            ),
            (
                spec.files[1].url.to_string(),
                Ok(response(spec.files[1].url, fixture.contexts)),
            ),
        ]);
        assert!(
            fetch_with_transport(&bad_transport, &spec, &output, &summary)
                .await
                .is_err()
        );
        assert!(!output.exists());
        assert_no_staging(temp.path());
    }

    #[tokio::test]
    async fn fetch_memory_benchmark_atomically_publishes_valid_package_and_summary() {
        // Pins: validation precedes publication and the strict summary records exact counts and provenance.
        let fixture = fixture();
        let spec = tiny_spec(&fixture);
        let transport = FakeTransport::new([
            (
                spec.files[0].url.to_string(),
                Ok(response(spec.files[0].url, fixture.questions)),
            ),
            (
                spec.files[1].url.to_string(),
                Ok(response(spec.files[1].url, fixture.contexts)),
            ),
        ]);
        let temp = TempDir::new().expect("tempdir");
        let (output, summary) = target_paths(temp.path());
        let loaded = fetch_with_transport(&transport, &spec, &output, &summary)
            .await
            .expect("valid package should publish");
        let VerifiedFetchSummaryV1::PersonaMem(loaded) = loaded else {
            panic!("expected PersonaMem fetch summary")
        };
        assert_eq!(loaded.question_count, 3);
        assert_eq!(loaded.persona_count, 2);
        assert_eq!(loaded.context_count, 2);
        assert!(output.join("package.json").is_file());
        let summary_value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&summary).expect("read fetch summary"))
                .expect("parse summary");
        assert_eq!(summary_value["verified"], true);
        assert_eq!(summary_value["question_count"], 3);
        assert_no_staging(temp.path());
        assert!(!summary.with_extension("json.tmp").exists());
    }

    #[tokio::test]
    async fn fetch_memory_benchmark_valid_destination_revalidates_without_network() {
        // Pins: an already-valid destination is a no-op fetch but still gets an atomically refreshed summary.
        let fixture = fixture();
        let spec = tiny_spec(&fixture);
        let temp = TempDir::new().expect("tempdir");
        let (output, summary) = target_paths(temp.path());
        let transport = FakeTransport::new([
            (
                spec.files[0].url.to_string(),
                Ok(response(spec.files[0].url, fixture.questions)),
            ),
            (
                spec.files[1].url.to_string(),
                Ok(response(spec.files[1].url, fixture.contexts)),
            ),
        ]);
        fetch_with_transport(&transport, &spec, &output, &summary)
            .await
            .expect("initial fetch");
        std::fs::remove_file(&summary).expect("remove summary");
        fetch_with_transport(&FakeTransport::no_requests(), &spec, &output, &summary)
            .await
            .expect("valid destination should revalidate without requests");
        assert!(summary.is_file());
        assert_no_staging(temp.path());
    }

    #[tokio::test]
    async fn fetch_memory_benchmark_rejects_status_untrusted_urls_and_summary_drift() {
        // Pins: final responses stay successful, credential-free HTTPS on the redirect allowlist.
        let fixture = fixture();
        let spec = tiny_spec(&fixture);
        for (status, final_url) in [
            (500, spec.files[0].url),
            (200, "http://huggingface.co/fixture/questions_32k.csv"),
            (
                200,
                "https://user:secret@huggingface.co/fixture/questions_32k.csv",
            ),
            (200, "https://example.com/fixture/questions_32k.csv"),
        ] {
            let transport = FakeTransport::new([(
                spec.files[0].url.to_string(),
                Ok(DownloadResponse {
                    status,
                    final_url: final_url.to_string(),
                    bytes: fixture.questions.clone(),
                }),
            )]);
            let temp = TempDir::new().expect("tempdir");
            let (output, summary) = target_paths(temp.path());
            assert!(
                fetch_with_transport(&transport, &spec, &output, &summary)
                    .await
                    .is_err()
            );
            assert!(!output.exists());
            assert_no_staging(temp.path());
        }

        let mut unknown = serde_json::to_value(PersonaMemFetchSummaryV1 {
            schema_version: 1,
            dataset: PERSONAMEM_DATASET.to_string(),
            repository: PERSONAMEM_REPOSITORY.to_string(),
            revision: PERSONAMEM_REVISION.to_string(),
            package_sha256: spec.package_sha256,
            question_count: 3,
            persona_count: 2,
            context_count: 2,
            verified: true,
        })
        .expect("serialize summary");
        unknown
            .as_object_mut()
            .expect("summary object")
            .insert("extra".to_string(), serde_json::json!(true));
        assert!(serde_json::from_value::<PersonaMemFetchSummaryV1>(unknown).is_err());
    }

    #[tokio::test]
    async fn fetch_memory_benchmark_longmemeval_publishes_strict_verified_summary() {
        // Pins: the LongMemEval variant strict-loads before publication and
        // emits only its 7/1/6 fixture counts and immutable provenance.
        let bytes = longmemeval_fixture();
        let spec = tiny_longmemeval_spec(&bytes);
        let transport = FakeTransport::new([(
            spec.files[0].url.to_string(),
            Ok(response(spec.files[0].url, bytes)),
        )]);
        let temp = TempDir::new().expect("tempdir");
        let output = temp.path().join("target/longmemeval");
        let summary = temp.path().join("target/longmemeval-summary.json");

        let loaded = fetch_with_transport(&transport, &spec, &output, &summary)
            .await
            .expect("valid LongMemEval fixture should publish");
        assert_eq!(loaded.question_count(), 7);
        let parsed: LongMemEvalFetchSummaryV1 =
            serde_json::from_slice(&std::fs::read(summary).expect("read LongMemEval summary"))
                .expect("strict LongMemEval summary");
        assert_eq!(parsed.abstention_count, 1);
        assert_eq!(parsed.retrieval_count, 6);
        let value = serde_json::to_value(parsed).expect("summary value");
        assert!(value.get("persona_count").is_none());
        assert!(output.join("package.json").is_file());
    }

    #[test]
    fn fetch_memory_benchmark_longmemeval_spec_pins_only_approved_url() {
        // Pins: callers can select dataset/revision, never an arbitrary URL.
        let spec = BenchmarkSpec::for_dataset(LONGMEMEVAL_DATASET, LONGMEMEVAL_REVISION)
            .expect("pinned LongMemEval spec");
        assert_eq!(spec.files.len(), 1);
        assert_eq!(
            spec.files[0].url,
            "https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/98d7416c24c778c2fee6e6f3006e7a073259d48f/longmemeval_s_cleaned.json?download=true"
        );
        assert!(BenchmarkSpec::for_dataset(LONGMEMEVAL_DATASET, "main").is_err());
    }

    struct FixtureBytes {
        questions: Vec<u8>,
        contexts: Vec<u8>,
    }

    fn fixture() -> FixtureBytes {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../moa-eval/tests/fixtures/external_memory/personamem");
        FixtureBytes {
            questions: std::fs::read(root.join("questions_32k_tiny.csv"))
                .expect("read question fixture"),
            contexts: std::fs::read(root.join("shared_contexts_32k_tiny.jsonl"))
                .expect("read context fixture"),
        }
    }

    fn longmemeval_fixture() -> Vec<u8> {
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join(
                "../moa-eval/tests/fixtures/external_memory/longmemeval/longmemeval_s_cleaned_tiny.json",
            ),
        )
        .expect("read LongMemEval fixture")
    }

    fn tiny_longmemeval_spec(bytes: &[u8]) -> BenchmarkSpec {
        let file = file_spec(
            LONGMEMEVAL_FILE,
            "https://huggingface.co/fixture/longmemeval_s_cleaned.json",
            bytes,
        );
        let manifest = DatasetPackageManifestV1 {
            schema_version: 1,
            dataset: LONGMEMEVAL_DATASET.to_string(),
            source: DatasetPackageSourceV1 {
                repository: LONGMEMEVAL_REPOSITORY.to_string(),
                revision: LONGMEMEVAL_REVISION.to_string(),
            },
            files: vec![file.provenance()],
        };
        BenchmarkSpec {
            package_sha256: manifest.canonical_hash().expect("fixture package hash"),
            manifest,
            files: vec![file],
            validator: BenchmarkValidator::LongMemEval {
                question_count: 7,
                abstention_count: 1,
                retrieval_count: 6,
            },
        }
    }

    fn tiny_spec(fixture: &FixtureBytes) -> BenchmarkSpec {
        let files = vec![
            file_spec(
                "questions_32k.csv",
                "https://huggingface.co/fixture/questions_32k.csv",
                &fixture.questions,
            ),
            file_spec(
                "shared_contexts_32k.jsonl",
                "https://huggingface.co/fixture/shared_contexts_32k.jsonl",
                &fixture.contexts,
            ),
        ];
        let manifest = DatasetPackageManifestV1 {
            schema_version: 1,
            dataset: PERSONAMEM_DATASET.to_string(),
            source: DatasetPackageSourceV1 {
                repository: PERSONAMEM_REPOSITORY.to_string(),
                revision: PERSONAMEM_REVISION.to_string(),
            },
            files: files.iter().map(BenchmarkFileSpec::provenance).collect(),
        };
        let package_sha256 = manifest.canonical_hash().expect("hash tiny manifest");
        BenchmarkSpec {
            manifest,
            package_sha256,
            files,
            validator: BenchmarkValidator::PersonaMem32k {
                question_count: 3,
                persona_count: 2,
                context_count: 2,
            },
        }
    }

    fn file_spec(path: &'static str, url: &'static str, bytes: &[u8]) -> BenchmarkFileSpec {
        BenchmarkFileSpec {
            path,
            url,
            size_bytes: u64::try_from(bytes.len()).expect("fixture length fits u64"),
            sha256: format!("{:x}", Sha256::digest(bytes)),
        }
    }

    fn response(url: &str, bytes: Vec<u8>) -> DownloadResponse {
        DownloadResponse {
            status: 200,
            final_url: url.to_string(),
            bytes,
        }
    }

    fn target_paths(root: &Path) -> (PathBuf, PathBuf) {
        let target = root.join("target");
        (
            target.join("personamem-32k"),
            target.join("personamem-32k-summary.json"),
        )
    }

    fn assert_no_staging(root: &Path) {
        let target = root.join("target");
        if !target.exists() {
            return;
        }
        let staging = std::fs::read_dir(target)
            .expect("read target")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("staging"))
            .collect::<Vec<_>>();
        assert!(staging.is_empty(), "staging leaked: {staging:?}");
    }
}
