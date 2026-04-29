use anyhow::{Result, bail};

pub(super) struct LxcImage {
    pub(super) distribution: String,
    pub(super) release: String,
}

impl LxcImage {
    pub(super) fn parse(value: &str) -> Result<Self> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            bail!("container image must not be empty");
        }

        let Some((distribution, release)) = trimmed.split_once(':') else {
            bail!("container image must use distribution:release format, for example ubuntu:24.04");
        };
        let distribution = distribution.trim();
        let release = release.trim();
        if distribution.is_empty() || release.is_empty() {
            bail!("container image must use distribution:release format, for example ubuntu:24.04");
        }

        Ok(Self {
            distribution: distribution.to_string(),
            release: normalize_release(distribution, release).to_string(),
        })
    }
}

fn normalize_release<'a>(distribution: &str, release: &'a str) -> &'a str {
    match (distribution, release) {
        ("ubuntu", "24.04") => "noble",
        ("ubuntu", "22.04") => "jammy",
        ("ubuntu", "20.04") => "focal",
        _ => release,
    }
}

pub(super) fn lxc_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        arch => arch,
    }
}
