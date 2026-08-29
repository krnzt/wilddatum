use async_trait::async_trait;
use ecoscope_core::{
    CatalogEntry, CatalogQuery, CredentialRef, DatasetManifest, DatasetPlan, DatasetRequest,
    ProviderCapability, ProviderManifest, ResourceQuery, ResourceRecord, Result,
};
use std::collections::BTreeSet;

pub const PROVIDER_PROTOCOL_VERSION: u32 = 2;

pub fn validate_manifest(manifest: &ProviderManifest) -> Result<()> {
    if manifest.schema_version != PROVIDER_PROTOCOL_VERSION {
        return Err(ecoscope_core::EcoScopeError::Invalid(format!(
            "provider {} uses unsupported schema version {}",
            manifest.provider_id, manifest.schema_version
        )));
    }
    if manifest.provider_id.is_empty()
        || !manifest
            .provider_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(ecoscope_core::EcoScopeError::Invalid(
            "provider_id must contain only lowercase ASCII letters, digits, and hyphens".into(),
        ));
    }
    if manifest.capabilities.is_empty() {
        return Err(ecoscope_core::EcoScopeError::Invalid(
            "provider must advertise at least one capability".into(),
        ));
    }
    let unique = manifest
        .capabilities
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if unique.len() != manifest.capabilities.len() {
        return Err(ecoscope_core::EcoScopeError::Invalid(
            "provider capabilities must be unique".into(),
        ));
    }
    for origin in &manifest.allowed_network_origins {
        if !origin.starts_with("https://") || origin.ends_with('/') {
            return Err(ecoscope_core::EcoScopeError::Invalid(format!(
                "provider origin must be an HTTPS origin without a trailing slash: {origin}"
            )));
        }
    }
    Ok(())
}

#[async_trait]
pub trait EcologicalDataProvider: Send + Sync {
    fn provider_id(&self) -> &str;

    fn manifest(&self) -> ProviderManifest;

    fn supports(&self, capability: ProviderCapability) -> bool {
        self.manifest().capabilities.contains(&capability)
    }

    async fn search_catalog(&self, query: CatalogQuery) -> Result<Vec<CatalogEntry>>;

    async fn inspect_product(&self, id: &str) -> Result<CatalogEntry>;

    async fn search_resources(&self, query: ResourceQuery) -> Result<Vec<ResourceRecord>> {
        let sites = query
            .provider_filters
            .get("sites")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect();
        self.search_catalog(CatalogQuery {
            text: query.text,
            modalities: query.modalities,
            sites,
            start_month: query.temporal_start,
            end_month: query.temporal_end,
            limit: query.limit,
        })
        .await
        .map(|records| records.into_iter().map(ResourceRecord::from).collect())
    }

    async fn resolve_resource(&self, id: &str) -> Result<ResourceRecord> {
        self.inspect_product(id).await.map(ResourceRecord::from)
    }

    async fn plan_dataset(&self, request: DatasetRequest) -> Result<DatasetPlan>;

    async fn materialize(
        &self,
        plan: DatasetPlan,
        credentials: Option<CredentialRef>,
    ) -> Result<DatasetManifest>;
}

#[cfg(test)]
mod tests {
    use ecoscope_core::{ProviderCapability, ProviderStatus};

    use super::*;

    fn valid_manifest() -> ProviderManifest {
        ProviderManifest {
            schema_version: PROVIDER_PROTOCOL_VERSION,
            provider_id: "tern".into(),
            name: "TERN fixture".into(),
            version: "0.1.0".into(),
            status: ProviderStatus::Community,
            capabilities: vec![ProviderCapability::CatalogSearch],
            allowed_network_origins: vec!["https://example.tern.org.au".into()],
            authentication: vec![],
            standards: vec!["Darwin Core".into()],
            homepage: None,
            support_url: None,
        }
    }

    #[test]
    fn accepts_language_neutral_provider_manifests() {
        assert!(validate_manifest(&valid_manifest()).is_ok());
        let serialized = serde_json::to_value(valid_manifest()).unwrap();
        assert_eq!(serialized["provider_id"], "tern");
        assert_eq!(serialized["capabilities"][0], "catalog_search");
    }

    #[test]
    fn rejects_unsafe_provider_network_contracts() {
        let mut manifest = valid_manifest();
        manifest.allowed_network_origins = vec!["http://insecure.example".into()];
        assert!(validate_manifest(&manifest).is_err());
        manifest = valid_manifest();
        manifest
            .capabilities
            .push(ProviderCapability::CatalogSearch);
        assert!(validate_manifest(&manifest).is_err());
    }
}
