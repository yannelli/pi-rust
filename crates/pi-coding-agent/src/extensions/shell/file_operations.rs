//! File operations supplied to SDK read, write, and edit tool factories.

use std::path::Path;

use anyhow::bail;
use async_trait::async_trait;
use pi_js::{HostValue, JsRef};

use crate::tools::{EditOperations, ReadOperations, WriteOperations};

pub(super) struct JsFileOperations(pub JsRef);

impl JsFileOperations {
    async fn call(&self, method: &str, path: &Path, content: Option<&str>) -> anyhow::Result<HostValue> {
        let mut args = vec![method.into(), path.to_string_lossy().into_owned().into()];
        if let Some(content) = content {
            args.push(content.into());
        }
        Ok(self.0.call(args).await?)
    }
}

#[async_trait]
impl ReadOperations for JsFileOperations {
    async fn read_file(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        match self.call("readFile", path, None).await? {
            HostValue::Bytes(bytes) => Ok(bytes),
            _ => bail!("readFile must return a Buffer"),
        }
    }

    async fn access(&self, path: &Path) -> anyhow::Result<()> {
        self.call("access", path, None).await?;
        Ok(())
    }

    async fn detect_image_mime_type(&self, path: &Path) -> anyhow::Result<Option<String>> {
        let mime = self.call("detectImageMimeType", path, None).await?.into_json();
        Ok(serde_json::from_value(mime)?)
    }
}

#[async_trait]
impl WriteOperations for JsFileOperations {
    async fn write_file(&self, path: &Path, content: &str) -> anyhow::Result<()> {
        self.call("writeFile", path, Some(content)).await?;
        Ok(())
    }

    async fn mkdir(&self, path: &Path) -> anyhow::Result<()> {
        self.call("mkdir", path, None).await?;
        Ok(())
    }
}

#[async_trait]
impl EditOperations for JsFileOperations {
    async fn read_file(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        ReadOperations::read_file(self, path).await
    }

    async fn write_file(&self, path: &Path, content: &str) -> anyhow::Result<()> {
        WriteOperations::write_file(self, path, content).await
    }

    async fn access(&self, path: &Path) -> anyhow::Result<()> {
        ReadOperations::access(self, path).await
    }
}
