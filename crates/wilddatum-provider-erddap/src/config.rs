#[derive(Debug, Clone, Copy)]
pub struct ErddapPreset {
    pub provider_id: &'static str,
    pub name: &'static str,
    pub base_url: &'static str,
    pub allowed_origin: &'static str,
    pub homepage: &'static str,
    pub catalog_scope: Option<&'static str>,
}

pub const PRESETS: [ErddapPreset; 3] = [
    ErddapPreset {
        provider_id: "emso",
        name: "EMSO ERIC",
        base_url: "https://erddap.emso.eu/erddap",
        allowed_origin: "https://erddap.emso.eu",
        homepage: "https://emso.eu/",
        catalog_scope: None,
    },
    ErddapPreset {
        provider_id: "icos-erddap",
        name: "ICOS Carbon Portal ERDDAP",
        base_url: "https://erddap.icos-cp.eu/erddap",
        allowed_origin: "https://erddap.icos-cp.eu",
        homepage: "https://www.icos-cp.eu/",
        catalog_scope: None,
    },
    ErddapPreset {
        provider_id: "euro-argo",
        name: "Euro-Argo / Argo at Ifremer",
        base_url: "https://erddap.ifremer.fr/erddap",
        allowed_origin: "https://erddap.ifremer.fr",
        homepage: "https://www.euro-argo.eu/",
        catalog_scope: Some("Argo"),
    },
];

pub fn presets() -> &'static [ErddapPreset] {
    &PRESETS
}

pub fn preset(provider_id: &str) -> Option<ErddapPreset> {
    PRESETS
        .iter()
        .copied()
        .find(|preset| preset.provider_id == provider_id)
}

#[derive(Debug, Clone)]
pub struct ErddapConfig {
    pub provider_id: String,
    pub name: String,
    pub base_url: String,
    pub allowed_origin: String,
    pub homepage: String,
    pub catalog_scope: Option<String>,
}

impl From<ErddapPreset> for ErddapConfig {
    fn from(preset: ErddapPreset) -> Self {
        Self {
            provider_id: preset.provider_id.into(),
            name: preset.name.into(),
            base_url: preset.base_url.into(),
            allowed_origin: preset.allowed_origin.into(),
            homepage: preset.homepage.into(),
            catalog_scope: preset.catalog_scope.map(str::to_owned),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn shipped_presets_have_unique_ids_and_https_origins() {
        let presets = presets();
        assert_eq!(presets.len(), 3);
        let ids = presets
            .iter()
            .map(|preset| preset.provider_id)
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), presets.len());
        for preset in presets {
            assert!(preset.base_url.starts_with("https://"));
            assert!(preset.base_url.ends_with("/erddap"));
            assert!(!preset.allowed_origin.ends_with('/'));
        }
    }
}
