//! AWS partition DNS suffixes, from the SDK's `partitions.json`.

/// `(region prefix, dns suffix, dual-stack dns suffix)`, most specific prefixes first.
const PARTITIONS: [(&str, &str, &str); 7] = [
    ("us-isob-", "sc2s.sgov.gov", "api.aws.scloud"),
    ("us-isof-", "csp.hci.ic.gov", "api.aws.hci.ic.gov"),
    ("us-iso-", "c2s.ic.gov", "api.aws.ic.gov"),
    ("eu-isoe-", "cloud.adc-e.uk", "api.cloud-aws.adc-e.uk"),
    ("eusc-", "amazonaws.eu", "api.amazonwebservices.eu"),
    ("cn-", "amazonaws.com.cn", "api.amazonwebservices.com.cn"),
    ("us-gov-", "amazonaws.com", "api.aws"),
];

fn partition(region: &str) -> (&'static str, &'static str) {
    PARTITIONS
        .iter()
        .find(|(prefix, _, _)| region.starts_with(prefix))
        .map_or(("amazonaws.com", "api.aws"), |(_, suffix, dual)| (*suffix, *dual))
}

/// The DNS suffix of the partition that holds `region`.
pub fn dns_suffix(region: &str) -> &'static str {
    partition(region).0
}

/// `{service}.{region}.{suffix}`.
pub fn service_host(service: &str, region: &str) -> String {
    format!("{service}.{region}.{}", dns_suffix(region))
}

/// The regional endpoint for a service, honoring FIPS and dual-stack choices.
pub fn regional_endpoint(service: &str, region: &str, fips: bool, dual_stack: bool) -> String {
    let (suffix, dual_suffix) = partition(region);
    let service = if fips { format!("{service}-fips") } else { service.to_string() };
    format!("https://{service}.{region}.{}", if dual_stack { dual_suffix } else { suffix })
}

/// The region of a standard `bedrock-runtime[-fips].{region}.amazonaws.com[.cn]` URL.
pub fn standard_bedrock_endpoint_region(base_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(base_url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let rest = host.strip_prefix("bedrock-runtime-fips.").or_else(|| host.strip_prefix("bedrock-runtime."))?;
    let region = rest.strip_suffix(".amazonaws.com").or_else(|| rest.strip_suffix(".amazonaws.com.cn"))?;
    let valid = !region.is_empty() && region.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    valid.then(|| region.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_partition_hosts() {
        assert_eq!(
            regional_endpoint("bedrock-runtime", "us-east-1", false, false),
            "https://bedrock-runtime.us-east-1.amazonaws.com"
        );
        assert_eq!(
            regional_endpoint("bedrock-runtime", "cn-north-1", false, false),
            "https://bedrock-runtime.cn-north-1.amazonaws.com.cn"
        );
        assert_eq!(
            regional_endpoint("bedrock-runtime", "us-gov-west-1", true, false),
            "https://bedrock-runtime-fips.us-gov-west-1.amazonaws.com"
        );
        assert_eq!(
            regional_endpoint("bedrock-runtime", "eu-west-1", false, true),
            "https://bedrock-runtime.eu-west-1.api.aws"
        );
        assert_eq!(service_host("sts", "us-iso-east-1"), "sts.us-iso-east-1.c2s.ic.gov");
        assert_eq!(
            standard_bedrock_endpoint_region("https://bedrock-runtime.eu-central-1.amazonaws.com"),
            Some("eu-central-1".into())
        );
        assert_eq!(
            standard_bedrock_endpoint_region("https://bedrock-runtime-fips.cn-north-1.amazonaws.com.cn/"),
            Some("cn-north-1".into())
        );
        assert_eq!(standard_bedrock_endpoint_region("https://bedrock-vpc.example.com"), None);
    }
}
