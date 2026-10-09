use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::zen::ZenModel;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelInfo {
   pub slug: String,
   #[serde(default)]
   pub display_name: Option<String>,
   pub visibility: Option<String>,
   pub supported_in_api: Option<bool>,
   pub context_window: Option<i64>,
   #[serde(default)]
   pub input_modalities: Vec<String>,
   #[serde(default)]
   pub service_tiers: Vec<ServiceTier>,
   /// Provider-declared model speed variants, such as `fast`.
   #[serde(default)]
   pub additional_speed_tiers: Vec<String>,
   #[serde(flatten)]
   pub rest: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServiceTier {
   pub id: String,
   pub name: String,
   pub description: String,
   #[serde(flatten)]
   pub rest: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelsResponse {
   pub models: Vec<ModelInfo>,
   #[serde(flatten)]
   pub rest: BTreeMap<String, Value>,
}

impl ModelsResponse {
   /// Returns the catalog slug that can authorize a requested model.
   ///
   /// OpenAI occasionally exposes latency variants as a `-fast` suffix while
   /// the account catalog only advertises the underlying model. Keep the
   /// requested slug untouched for the upstream call, but let admission use
   /// the base capability when (and only when) that base is listed.
   pub fn supports_model(&self, requested: &str) -> bool {
      self.models.is_empty()
         || self.models.iter().any(|model| {
            model.slug == requested
               || fast_base_slug(requested).is_some_and(|base| model.slug == base)
         })
   }

   pub fn supports_tier(&self, requested: &str, tier: &str) -> bool {
      self.models.iter().any(|model| {
         (model.slug == requested
            || fast_base_slug(requested).is_some_and(|base| model.slug == base))
            && model.service_tiers.iter().any(|service| service.id == tier)
      })
   }

   /// Materialize provider-declared speed variants as discoverable model slugs.
   pub fn expand_speed_variants(&mut self) {
      let existing: std::collections::BTreeSet<String> =
         self.models.iter().map(|model| model.slug.clone()).collect();
      let mut variants = Vec::new();
      for model in &self.models {
         for speed in &model.additional_speed_tiers {
            let speed = speed.trim();
            if speed.is_empty() {
               continue;
            }
            let slug = format!("{}-{speed}", model.slug);
            if existing.contains(&slug)
               || variants.iter().any(|entry: &ModelInfo| entry.slug == slug)
            {
               continue;
            }
            let mut variant = model.clone();
            variant.slug = slug;
            variant.display_name = variant.display_name.map(|name| format!("{name} ({speed})"));
            variant.additional_speed_tiers.clear();
            variants.push(variant);
         }
      }
      self.models.extend(variants);
   }

   pub fn add_service_tier(&mut self, slug: &str, tier: ServiceTier) {
      if let Some(model) = self.models.iter_mut().find(|model| model.slug == slug)
         && !model
            .service_tiers
            .iter()
            .any(|existing| existing.id == tier.id)
      {
         model.service_tiers.push(tier);
      }
   }

   pub fn merge(&mut self, incoming: &Self) {
      for candidate in &incoming.models {
         if self.models.iter().any(|model| model.slug == candidate.slug) {
            for tier in &candidate.service_tiers {
               self.add_service_tier(&candidate.slug, tier.clone());
            }
         } else {
            self.models.push(candidate.clone());
         }
      }
   }
}

/// The provider's generic fast-model convention. We deliberately only accept
/// the complete `-fast` suffix, rather than guessing from arbitrary name
/// fragments or making up models for discovery.
pub fn fast_base_slug(requested: &str) -> Option<&str> {
   requested
      .strip_suffix("-fast")
      .filter(|base| !base.is_empty())
}

impl ModelInfo {
   /// Returns whether the backend reports this model as usable through the API.
   ///
   /// `visibility` is a client presentation hint. A hidden model can still be
   /// explicitly requested, so it must remain discoverable through the proxy.
   pub fn listed(&self) -> bool {
      self.supported_in_api != Some(false)
   }
}

/// Codex's fallback for an unlisted slug lets code mode declare a `custom` tool zen refuses.
#[derive(Serialize)]
struct ZenEntry<'a> {
   display_name: &'a str,
   description: &'static str,
   tool_mode: &'static str,
   shell_type: &'static str,
   web_search_tool_type: &'static str,
   apply_patch_tool_type: Option<()>,
   use_responses_lite: bool,
   prefer_websockets: bool,
   supports_search_tool: bool,
   experimental_supported_tools: [(); 0],
   service_tiers: [(); 0],
   additional_speed_tiers: [(); 0],
   priority: i32,
   upgrade: Option<()>,
   availability_nux: Option<()>,
   comp_hash: Option<()>,
   default_reasoning_level: &'static str,
}

const ZEN_EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];

#[derive(Clone, Deserialize, Serialize)]
struct CatalogEntry {
   #[serde(skip_serializing_if = "Option::is_none")]
   slug: Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   visibility: Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   supported_reasoning_levels: Option<Vec<ReasoningLevel>>,
   #[serde(skip_serializing_if = "Option::is_none")]
   context_window: Option<i64>,
   #[serde(flatten)]
   rest: BTreeMap<String, Value>,
}

#[derive(Clone, Deserialize, Serialize)]
struct ReasoningLevel {
   #[serde(skip_serializing_if = "Option::is_none")]
   effort: Option<String>,
   #[serde(flatten)]
   rest: BTreeMap<String, Value>,
}

#[derive(Deserialize, Serialize)]
struct Catalog {
   models: Vec<CatalogEntry>,
   #[serde(flatten)]
   rest: BTreeMap<String, Value>,
}

/// Cloned from `template` so the fields codex requires track the backend.
pub fn with_zen_entries<'zen, Zen>(raw: &str, template: &str, zen: Zen) -> Option<String>
where
   Zen: IntoIterator<Item = &'zen ZenModel>,
{
   let mut catalog: Catalog = serde_json::from_str(raw).ok()?;
   let present: Vec<String> = catalog
      .models
      .iter()
      .filter_map(|entry| entry.slug.clone())
      .collect();
   let base = catalog
      .models
      .iter()
      .find(|entry| entry.slug.as_deref() == Some(template))
      .or_else(|| {
         catalog
            .models
            .iter()
            .find(|entry| entry.visibility.as_deref() == Some("list"))
      })?
      .clone();
   let levels: Vec<ReasoningLevel> = base
      .supported_reasoning_levels
      .iter()
      .flatten()
      .filter(|level| {
         level
            .effort
            .as_deref()
            .is_some_and(|effort| ZEN_EFFORTS.contains(&effort))
      })
      .cloned()
      .collect();
   for model in zen {
      let id = &model.id;
      if present.contains(id) {
         continue;
      }
      let patch = serde_json::to_value(ZenEntry {
         display_name: id,
         description: "Served by opencode zen",
         tool_mode: "direct",
         shell_type: "unified_exec",
         web_search_tool_type: "text",
         apply_patch_tool_type: None,
         use_responses_lite: false,
         prefer_websockets: false,
         supports_search_tool: false,
         experimental_supported_tools: [],
         service_tiers: [],
         additional_speed_tiers: [],
         priority: 99,
         upgrade: None,
         availability_nux: None,
         comp_hash: None,
         default_reasoning_level: "high",
      })
      .ok()?;
      let mut entry = base.clone();
      entry.slug = Some(id.clone());
      entry.supported_reasoning_levels = Some(levels.clone());
      entry.context_window = model.context_window.or(base.context_window);
      if let Value::Object(fields) = patch {
         entry.rest.extend(fields);
      }
      catalog.models.push(entry);
   }
   serde_json::to_string(&catalog).ok()
}

#[cfg(test)]
mod tests {
   use super::ModelInfo;

   #[test]
   fn api_supported_hidden_models_remain_discoverable() {
      let model: ModelInfo = serde_json::from_str(
         r#"{"slug":"provider-new-model","visibility":"hide","supported_in_api":true}"#,
      )
      .unwrap();
      assert!(model.listed());
   }

   #[test]
   fn explicitly_unsupported_models_remain_hidden() {
      let model: ModelInfo = serde_json::from_str(
         r#"{"slug":"provider-internal-model","visibility":"list","supported_in_api":false}"#,
      )
      .unwrap();
      assert!(!model.listed());
   }
}

#[cfg(test)]
mod zen_entry_tests {
   use std::collections::BTreeMap;

   use super::{ModelsResponse, with_zen_entries};
   use crate::zen::ZenModel;

   fn zen(id: &str) -> ZenModel {
      ZenModel {
         id: id.to_owned(),
         context_window: None,
      }
   }

   const CATALOG: &str = r#"{"models":[
      {"slug":"gpt-5.6-sol","visibility":"list","tool_mode":"code_mode_only","apply_patch_tool_type":"freeform",
       "use_responses_lite":true,"prefer_websockets":true,"shell_type":"unified_exec","priority":1,
       "web_search_tool_type":"text_and_image",
       "supported_reasoning_levels":[{"effort":"low"},{"effort":"xhigh"},{"effort":"ultra"}],
       "model_messages":{"instructions_template":"be codex"}},
      {"slug":"muse-old","visibility":"list","tool_mode":"direct"}
   ]}"#;

   #[test]
   fn a_zen_model_inherits_the_template_with_function_tools_only() {
      let ids = [zen("muse-spark-1.3-contributor-free"), zen("muse-old")];
      let out = with_zen_entries(CATALOG, "gpt-5.6-sol", &ids).unwrap();
      let catalog: serde_json::Value = serde_json::from_str(&out).unwrap();
      let models = catalog["models"].as_array().unwrap();
      assert_eq!(models.len(), 3);
      let muse = &models[2];
      assert_eq!(muse["slug"], "muse-spark-1.3-contributor-free");
      assert_eq!(muse["tool_mode"], "direct");
      assert!(muse["apply_patch_tool_type"].is_null());
      assert_eq!(muse["use_responses_lite"], false);
      assert_eq!(muse["prefer_websockets"], false);
      assert_eq!(muse["web_search_tool_type"], "text");
      assert_eq!(muse["model_messages"]["instructions_template"], "be codex");
      let efforts: Vec<_> = muse["supported_reasoning_levels"]
         .as_array()
         .unwrap()
         .iter()
         .map(|level| level["effort"].as_str().unwrap())
         .collect();
      assert_eq!(efforts, ["low", "xhigh"]);
   }

   #[test]
   fn provider_speed_tiers_are_materialized_as_models() {
      let mut catalog: ModelsResponse = serde_json::from_str(
         r#"{"models":[{"slug":"gpt-6-astra","display_name":"GPT-6-Astra","additional_speed_tiers":["fast","priority"]}]}"#,
      )
      .unwrap();
      catalog.expand_speed_variants();
      assert!(
         catalog
            .models
            .iter()
            .any(|model| model.slug == "gpt-6-astra-fast")
      );
      assert!(
         catalog
            .models
            .iter()
            .any(|model| model.slug == "gpt-6-astra-priority")
      );
      assert_eq!(
         catalog
            .models
            .iter()
            .find(|model| model.slug == "gpt-6-astra-fast")
            .and_then(|model| model.display_name.as_deref()),
         Some("GPT-6-Astra (fast)")
      );
   }

   #[test]
   fn fast_variants_use_a_listed_base_for_capability_checks() {
      let catalog: ModelsResponse =
         serde_json::from_str(r#"{"models":[{"slug":"gpt-6-astra"}]}"#).unwrap();
      assert!(catalog.supports_model("gpt-6-astra"));
      assert!(catalog.supports_model("gpt-6-astra-fast"));
      assert!(!catalog.supports_model("gpt-6-astra-fast-preview"));
      assert!(!catalog.supports_model("gpt-7-astra-fast"));
   }

   #[test]
   fn fast_variants_inherit_tiers_from_their_listed_base() {
      let catalog: ModelsResponse = serde_json::from_str(
         r#"{"models":[{"slug":"gpt-6-astra","service_tiers":[{"id":"priority","name":"Priority","description":""}]}]}"#,
      )
      .unwrap();
      assert!(catalog.supports_tier("gpt-6-astra-fast", "priority"));
      assert!(!catalog.supports_tier("gpt-6-astra-fast", "standard"));
   }

   #[test]
   fn fast_variants_are_not_invented_for_empty_or_unrelated_catalogs() {
      let empty = ModelsResponse {
         models: Vec::new(),
         rest: BTreeMap::new(),
      };
      assert!(empty.supports_model("gpt-6-astra-fast"));
      let catalog: ModelsResponse =
         serde_json::from_str(r#"{"models":[{"slug":"gpt-6-astra-fast"}]}"#).unwrap();
      assert!(catalog.supports_model("gpt-6-astra-fast"));
      assert!(!catalog.supports_model("gpt-6-astra"));
   }

   #[test]
   fn a_catalog_without_a_listed_entry_is_left_alone() {
      let raw = r#"{"models":[{"slug":"x","visibility":"hide"}]}"#;
      assert!(with_zen_entries(raw, "gpt-5.6-sol", &[zen("muse")]).is_none());
   }
}
