use super::*;

impl ServerConfig {
    pub(crate) fn authorize_file(&self, file: &str) -> Result<PathBuf, MediaAccessError> {
        let path = fs::canonicalize(file).map_err(|_| MediaAccessError::NotAllowed)?;
        if self.media.allowed_file_roots.is_empty()
            || !self
                .media
                .allowed_file_roots
                .iter()
                .any(|root| path.starts_with(root))
        {
            return Err(MediaAccessError::NotAllowed);
        }
        if !path.is_file() {
            return Err(MediaAccessError::NotAllowed);
        }
        Ok(path)
    }

    pub(crate) async fn authorize_url(&self, source: &str) -> Result<String, MediaAccessError> {
        let url = url::Url::parse(source).map_err(|_| MediaAccessError::NotAllowed)?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(MediaAccessError::NotAllowed);
        }
        let scheme_allowed = match url.scheme() {
            "http" => self.media.allow_http,
            "https" => true,
            _ => false,
        };
        if !scheme_allowed {
            return Err(MediaAccessError::NotAllowed);
        }
        let host = url.host_str().ok_or(MediaAccessError::NotAllowed)?;
        if !host_allowed(host, &self.media.allowed_url_hosts) {
            return Err(MediaAccessError::NotAllowed);
        }
        let port = url
            .port_or_known_default()
            .ok_or(MediaAccessError::NotAllowed)?;
        if self.media.block_private_networks {
            let addrs = tokio::net::lookup_host((host, port))
                .await
                .map_err(|_| MediaAccessError::NotAllowed)?
                .collect::<Vec<_>>();
            if addrs.is_empty() || addrs.iter().any(|address| !is_public_ip(address.ip())) {
                return Err(MediaAccessError::NotAllowed);
            }
        }
        Ok(url.into())
    }
}
