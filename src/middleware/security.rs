use crate::http::{Method, Request};
use crate::middleware::{Middleware, MiddlewareResult, Next};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use lazy_static::lazy_static;
use crate::error::ServerError;
use crate::http::Response;

#[derive(Clone)]
pub struct SecurityConfig {
    pub hsts: bool,
    pub xss_protection: bool,
    pub content_type_options: bool,
    pub frame_options: Option<String>,
    pub content_security_policy: Option<String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            hsts: true,
            xss_protection: true,
            content_type_options: true,
            frame_options: Some("DENY".to_string()),
            content_security_policy: None,
        }
    }
}

pub struct SecurityHeaders {
    config: SecurityConfig,
}

impl SecurityHeaders {
    pub fn new(config: SecurityConfig) -> Self {
        Self { config }
    }
}

impl Middleware for SecurityHeaders {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        let config = self.config.clone();
        Box::pin(async move {
            let mut response = next.handle(req).await?;
            if config.hsts {
                response.headers.insert("Strict-Transport-Security".to_string(), "max-age=31536000".to_string());
            }
            if config.xss_protection {
                response.headers.insert("X-XSS-Protection".to_string(), "1; mode=block".to_string());
            }
            if config.content_type_options {
                response.headers.insert("X-Content-Type-Options".to_string(), "nosniff".to_string());
            }
            if let Some(ref frame_options) = config.frame_options {
                response.headers.insert("X-Frame-Options".to_string(), frame_options.clone());
            }
            if let Some(ref content_security_policy) = config.content_security_policy {
                response.headers.insert("Content-Security-Policy".to_string(), content_security_policy.clone());
            }

            Ok(response)
        })
    }

    fn clone_box(&self) -> Box<dyn Middleware> {
        Box::new(Self::new(self.config.clone()))
    }
}

#[derive(Clone)]
pub struct RateLimitConfig {
    pub requests_per_minute: u32,
    pub burst_size: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            requests_per_minute: 60,
            burst_size: 10,
        }
    }
}


lazy_static! {
    // Changed to store (IP, Path) combination
    static ref REQUESTS: Arc<Mutex<HashMap<(String, String), Vec<Instant>>>> =
        Arc::new(Mutex::new(HashMap::new()));
}

#[derive(Clone)]
pub struct RateLimiter {
    config: RateLimitConfig,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
        }
    }

    async fn is_allowed(&self, client_ip: &str, path: &str) -> bool {
        let mut requests = REQUESTS.lock().await;
        let now = Instant::now();
        let minute_ago = now - Duration::from_secs(60);
        let key = (client_ip.to_string(), path.to_string());

        // Clean up old requests
        if let Some(times) = requests.get_mut(&key) {
            times.retain(|&time| time > minute_ago);

            if times.len() >= self.config.burst_size as usize {
                return false;
            }

            if times.len() as u32 >= self.config.requests_per_minute {
                return false;
            }

            times.push(now);
        } else {
            requests.insert(key, vec![now]);
        }

        true
    }
}

impl Middleware for RateLimiter {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        let self_clone = self.clone();
        Box::pin(async move {
            let client_ip = req.headers.get("x-forwarded-for")
                .or_else(|| req.headers.get("x-real-ip"))
                .unwrap_or(&"unknown".to_string())
                .to_string();

            let path = req.path.clone();
            if self_clone.is_allowed(&client_ip, &path).await {
                next.handle(req).await
            } else {
                Err(ServerError::TooManyRequests)
            }
        })
    }

    fn clone_box(&self) -> Box<dyn Middleware> {
        Box::new(Self::new(self.config.clone()))
    }
}

#[derive(Clone)]
pub struct CorsConfig {
    pub allow_origins: Vec<String>,
    pub allow_methods: Vec<String>,
    pub allow_headers: Vec<String>,
    pub allow_credentials: bool,
    pub max_age: Option<u32>,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allow_origins: vec!["*".to_string()],
            allow_methods: vec!["GET".to_string(), "POST".to_string(), "PUT".to_string(), "DELETE".to_string(), "OPTIONS".to_string()],
            allow_headers: vec!["Content-Type".to_string(), "Authorization".to_string()],
            allow_credentials: false,
            max_age: Some(86400),
        }
    }
}

pub struct Cors {
    config: CorsConfig,
}

impl Cors {
    pub fn new(config: CorsConfig) -> Self {
        Self { config }
    }
}

impl Middleware for Cors {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        let config = self.config.clone();
        Box::pin(async move {
            let origin = req.headers.get("origin").cloned();

            if req.method == Method::OPTIONS {
                let mut response = Response::new(204);
                
                if let Some(origin) = origin {
                    if config.allow_origins.contains(&"*".to_string()) || config.allow_origins.contains(&origin) {
                        response.headers.insert("Access-Control-Allow-Origin".to_string(), origin);
                    }
                }
                
                let methods = config.allow_methods.join(", ");
                response.headers.insert("Access-Control-Allow-Methods".to_string(), methods);
                
                let headers = config.allow_headers.join(", ");
                response.headers.insert("Access-Control-Allow-Headers".to_string(), headers);
                
                if config.allow_credentials {
                    response.headers.insert("Access-Control-Allow-Credentials".to_string(), "true".to_string());
                }
                
                if let Some(max_age) = config.max_age {
                    response.headers.insert("Access-Control-Max-Age".to_string(), max_age.to_string());
                }
                
                return Ok(response);
            }

            let mut response = next.handle(req).await?;
            
            if let Some(origin) = origin {
                if config.allow_origins.contains(&"*".to_string()) || config.allow_origins.contains(&origin) {
                    response.headers.insert("Access-Control-Allow-Origin".to_string(), origin);
                }
            }
            
            if config.allow_credentials {
                response.headers.insert("Access-Control-Allow-Credentials".to_string(), "true".to_string());
            }
            
            Ok(response)
        })
    }

    fn clone_box(&self) -> Box<dyn Middleware> {
        Box::new(Self::new(self.config.clone()))
    }
}
