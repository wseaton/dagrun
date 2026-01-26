//! Environment variable and dotenv support

use std::collections::HashMap;
use std::path::Path;
use tracing::{info, warn};

use dagrun_ast::{DotenvSettings, ReadinessCheck, ServiceKind};

/// Load environment variables from dotenv files
pub fn load_dotenv(settings: &DotenvSettings) -> Result<(), String> {
    if !settings.load {
        return Ok(());
    }

    let paths = if settings.paths.is_empty() {
        vec![".env".to_string()]
    } else {
        settings.paths.clone()
    };

    for path in &paths {
        let p = Path::new(path);
        if p.exists() {
            match dotenvy::from_path(p) {
                Ok(_) => info!(path = %path, "loaded dotenv file"),
                Err(e) => {
                    if settings.required {
                        return Err(format!("failed to load {}: {}", path, e));
                    }
                    warn!(path = %path, error = %e, "failed to load dotenv file");
                }
            }
        } else if settings.required {
            return Err(format!("dotenv file not found: {}", path));
        }
    }

    Ok(())
}

/// Generate environment variables for a ready service
/// If forwarded_port is Some, it means we have a tunnel and should expose the local endpoint
pub fn service_env_vars(
    name: &str,
    kind: &ServiceKind,
    ready: Option<&ReadinessCheck>,
    forwarded_port: Option<u16>,
) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    let prefix = format!("DAGRUN_SVC_{}", name.to_uppercase().replace('-', "_"));

    // indicate the service is ready
    vars.insert(format!("{}_READY", prefix), "1".to_string());

    // indicate the kind
    let kind_str = match kind {
        ServiceKind::Managed => "managed",
        ServiceKind::External => "external",
    };
    vars.insert(format!("{}_KIND", prefix), kind_str.to_string());

    // add host/port/url based on readiness check type
    if let Some(ready) = ready {
        if let Some(local_port) = forwarded_port {
            // service is tunneled, expose the local tunnel endpoint
            vars.insert(format!("{}_HOST", prefix), "127.0.0.1".to_string());
            vars.insert(format!("{}_PORT", prefix), local_port.to_string());

            // rewrite URL if it's an HTTP check (strip path for base URL)
            if let ReadinessCheck::Http { url } = ready
                && let Ok(mut parsed) = url::Url::parse(url)
            {
                let _ = parsed.set_host(Some("127.0.0.1"));
                let _ = parsed.set_port(Some(local_port));
                parsed.set_path("");
                let base = parsed.to_string().trim_end_matches('/').to_string();
                vars.insert(format!("{}_URL", prefix), base.clone());
                vars.insert(format!("{}_BASE_URL", prefix), base);
            }
        } else {
            // no tunnel, use the original values
            if let Some((host, port)) = ready.host_port() {
                vars.insert(format!("{}_HOST", prefix), host);
                vars.insert(format!("{}_PORT", prefix), port.to_string());
            }
            if let Some(url) = ready.base_url() {
                vars.insert(format!("{}_URL", prefix), url.clone());
                vars.insert(format!("{}_BASE_URL", prefix), url);
            }
        }
    }

    vars
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_env_vars_http() {
        let ready = ReadinessCheck::Http {
            url: "http://localhost:8080/health".to_string(),
        };
        let vars = service_env_vars("api-server", &ServiceKind::Managed, Some(&ready), None);

        assert_eq!(
            vars.get("DAGRUN_SVC_API_SERVER_HOST"),
            Some(&"localhost".to_string())
        );
        assert_eq!(
            vars.get("DAGRUN_SVC_API_SERVER_PORT"),
            Some(&"8080".to_string())
        );
        assert_eq!(
            vars.get("DAGRUN_SVC_API_SERVER_URL"),
            Some(&"http://localhost:8080".to_string())
        );
        assert_eq!(
            vars.get("DAGRUN_SVC_API_SERVER_READY"),
            Some(&"1".to_string())
        );
    }

    #[test]
    fn test_service_env_vars_tcp() {
        let ready = ReadinessCheck::Tcp {
            host: "localhost".to_string(),
            port: 5432,
        };
        let vars = service_env_vars("postgres", &ServiceKind::External, Some(&ready), None);

        assert_eq!(
            vars.get("DAGRUN_SVC_POSTGRES_HOST"),
            Some(&"localhost".to_string())
        );
        assert_eq!(
            vars.get("DAGRUN_SVC_POSTGRES_PORT"),
            Some(&"5432".to_string())
        );
        assert_eq!(
            vars.get("DAGRUN_SVC_POSTGRES_KIND"),
            Some(&"external".to_string())
        );
    }

    #[test]
    fn test_service_env_vars_forwarded() {
        let ready = ReadinessCheck::Http {
            url: "http://localhost:8080/health".to_string(),
        };
        let vars = service_env_vars(
            "api-server",
            &ServiceKind::Managed,
            Some(&ready),
            Some(54321),
        );

        // when forwarded, should use the local tunnel endpoint with path stripped
        assert_eq!(
            vars.get("DAGRUN_SVC_API_SERVER_HOST"),
            Some(&"127.0.0.1".to_string())
        );
        assert_eq!(
            vars.get("DAGRUN_SVC_API_SERVER_PORT"),
            Some(&"54321".to_string())
        );
        assert_eq!(
            vars.get("DAGRUN_SVC_API_SERVER_URL"),
            Some(&"http://127.0.0.1:54321".to_string())
        );
        assert_eq!(
            vars.get("DAGRUN_SVC_API_SERVER_BASE_URL"),
            Some(&"http://127.0.0.1:54321".to_string())
        );
    }

    #[test]
    fn test_service_env_vars_base_url() {
        let ready = ReadinessCheck::Http {
            url: "http://localhost:8080/api/health".to_string(),
        };
        let vars = service_env_vars("web-api", &ServiceKind::Managed, Some(&ready), None);

        // _URL and _BASE_URL should both be base URL (no path)
        assert_eq!(
            vars.get("DAGRUN_SVC_WEB_API_URL"),
            Some(&"http://localhost:8080".to_string())
        );
        assert_eq!(
            vars.get("DAGRUN_SVC_WEB_API_BASE_URL"),
            Some(&"http://localhost:8080".to_string())
        );
    }
}
