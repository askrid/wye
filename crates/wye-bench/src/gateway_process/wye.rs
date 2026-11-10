//! How a Wye under test is started: its config for each [`Variant`], the
//! gateway token the authenticated variants present, and the keys in its
//! environment.

use std::path::Path;

use gateway_core::token::{self, NewToken};
use ring::signature::KeyPair;

use super::{
    Gateway, Variant,
    launch::{Launch, Setup},
};
use crate::client::Target;

/// Provider key variable for the variants whose upstream holds a key.
const BENCH_PROVIDER_KEY_ENV: &str = "WYE_BENCH_PROVIDER_KEY";
/// Hash key variable for the cache-diagnostics variant.
const BENCH_HASH_KEY_ENV: &str = "WYE_BENCH_HASH_KEY";
/// [`Variant::Refused`]'s first upstream rejects one request in this many.
const REFUSE_EVERY: u64 = 4;

/// A throwaway signing key's public half and one gateway token it signed.
struct BenchToken {
    public_key: String,
    token: String,
}

impl BenchToken {
    /// With `models`, the token carries that `models` claim.
    fn generate(models: Option<&[&str]>) -> Self {
        let key = wye_testkit::ed25519_keypair();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let token = token::issue(
            &key,
            &NewToken {
                sub: "bench",
                aud: "wye-bench",
                iat: now,
                exp: now + 3_600,
                models,
                max_output_tokens: None,
            },
        );
        Self {
            public_key: token::encode_public_key(key.public_key().as_ref()),
            token,
        }
    }
}

impl Variant {
    /// Whether the upstream holds a provider key instead of forwarding the
    /// caller's credential: `[auth]` excludes passthrough, and the other
    /// providers' routes and failover take a key.
    fn provider_key(self) -> bool {
        !matches!(self, Self::Plain | Self::Diag | Self::Otlp)
    }
}

impl Launch {
    pub(super) async fn wye(program: &str, variant: Variant, setup: &Setup<'_>) -> Self {
        // The refusing first upstream, spawned for this process only.
        let refusing = match variant {
            Variant::Refused => Some(format!(
                "http://{}",
                crate::mock::spawn_refusing(REFUSE_EVERY).await
            )),
            _ => None,
        };
        // Requests name model "anthropic/bench" (Bifrost relays bytes unchanged
        // only for a provider-qualified model): allowed by both lists.
        let token = matches!(variant, Variant::Auth | Variant::Models).then(|| {
            BenchToken::generate((variant == Variant::Models).then_some(&["anthropic/bench"][..]))
        });
        let mut env = Vec::new();
        if variant.provider_key() {
            // The mock ignores the key.
            env.push((BENCH_PROVIDER_KEY_ENV, "sk-bench-provider".to_owned()));
        }
        if variant == Variant::Diag {
            env.push((BENCH_HASH_KEY_ENV, "b".repeat(64)));
        }
        let config = wye_config(
            setup.port,
            setup.upstream,
            setup.ca_file.as_deref(),
            variant,
            token.as_ref().map(|t| t.public_key.as_str()),
            refusing.as_deref(),
        );
        Self {
            program: program.into(),
            args: vec![
                "serve".into(),
                "--config".into(),
                setup.config(Gateway::Wye(program, variant), "toml", config),
            ],
            env,
            health: "/healthz",
            target: match (&token, variant) {
                (Some(t), _) => Target::gateway(
                    setup.addr,
                    "",
                    vec![("authorization", format!("Bearer {}", t.token))],
                ),
                (None, Variant::Openai) => Target::plain(setup.addr).with_responses(),
                (None, _) => Target::plain(setup.addr),
            },
        }
    }
}

/// Wye's config for `variant` in front of `upstream_base_url`: `public_key`
/// is the signing key `[auth]` trusts, `refusing` the first upstream of
/// [`Variant::Refused`].
fn wye_config(
    port: u16,
    upstream_base_url: &str,
    ca_file: Option<&Path>,
    variant: Variant,
    public_key: Option<&str>,
    refusing: Option<&str>,
) -> String {
    let credential = if variant.provider_key() {
        format!("provider_key = {{ env = \"{BENCH_PROVIDER_KEY_ENV}\" }}")
    } else {
        "forward_caller_credential = true".to_owned()
    };
    let upstream = match variant {
        // Requests go to `/v1/responses`.
        Variant::Openai => "[[upstreams]]\nprovider = \"openai\"\nid = \"openai\"".to_owned(),
        // Model ids get the provider's prefix.
        Variant::Bedrock => "[[upstreams]]\nid = \"bedrock\"\nprovider = \"bedrock\"".to_owned(),
        // Two upstreams (both the mock, unless refusing), which fail over:
        // every request body is kept for resending.
        Variant::Failover | Variant::Refused => {
            let (first, standby) = match refusing {
                Some(url) => (url, "\nstandby = true"),
                None => (upstream_base_url, ""),
            };
            format!(
                "[[upstreams]]\nid = \"a\"\nbase_url = \"{first}\"\n{credential}\n\
                 [[upstreams]]\nid = \"b\"{standby}"
            )
        }
        _ => "[[upstreams]]".to_owned(),
    };
    let mut config = format!(
        "[server]\nlisten = \"127.0.0.1:{port}\"\nshutdown_grace = \"0s\"\n\n\
         {upstream}\nbase_url = \"{upstream_base_url}\"\n{credential}\n"
    );
    if let Some(path) = ca_file {
        config.push_str(&format!("ca_file = {:?}\n", path.display().to_string()));
    }
    // Tuning experiments: extra lines in the last upstream entry without
    // code changes.
    if let Ok(extra) = std::env::var("WYE_BENCH_UPSTREAM") {
        config.push_str(&extra);
        config.push('\n');
    }
    if let Some(key) = public_key {
        let models = match variant {
            Variant::Models => r#"models = ["anthropic/bench*", "claude-*"]"#,
            _ => "",
        };
        config.push_str(&format!(
            "\n[auth]\naudience = \"wye-bench\"\nsigning_keys = [\"{key}\"]\n{models}\n"
        ));
    }
    if variant == Variant::Diag {
        config.push_str(&format!(
            "\n[cache_diagnostics]\nhash_key = {{ env = \"{BENCH_HASH_KEY_ENV}\" }}\n"
        ));
    }
    if variant == Variant::Otlp {
        // The mock answers the export POSTs like any request.
        config.push_str(&format!(
            "\n[telemetry]\notlp_endpoint = \"{upstream_base_url}\"\n"
        ));
    }
    config
}
