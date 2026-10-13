//! Facet bundles: manifests, artifacts, loaders, and (with the `js` feature) the bundler.

mod loader;
mod manifest;

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
