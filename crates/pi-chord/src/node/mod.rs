//! Facet bundles: manifests, artifacts, loaders, and (with the `js` feature) the bundler.

#[cfg(feature = "js")]
mod bundler;
#[cfg(feature = "js")]
mod js;
mod loader;
mod manifest;
#[cfg(feature = "js")]
mod package;

#[cfg(feature = "js")]
pub use bundler::{BundleFacetsOptions, BundleFacetsResult, FacetBundlePlatform, bundle_facets};
#[cfg(feature = "js")]
pub use js::{JsObject, QuickJsFacetEvaluator};
pub use loader::{
    ExportedFacet, FacetBundleArtifactLoaderOptions, FacetBundleExternalResolver, FacetBundleLoaderOptions,
    FacetModule, FacetModuleEvaluator, ModuleExports, create_facet_bundle_artifact_loader, create_facet_bundle_loader,
    read_facet_bundle_artifact, read_facet_bundle_manifest,
};
pub use manifest::{
    FACET_BUNDLE_ARTIFACT_FORMAT, FACET_BUNDLE_ARTIFACT_FORMAT_VERSION, FACET_BUNDLE_FORMAT,
    FACET_BUNDLE_FORMAT_VERSION, FACET_BUNDLE_MANIFEST_FILE, FacetBundleArtifact, FacetBundleEntry,
    FacetBundleManifest, FacetBundlePlugin, integrity_of,
};
#[cfg(feature = "js")]
pub use package::{BundleFacetPackageOptions, BundleFacetPackageResult, bundle_facet_package};
