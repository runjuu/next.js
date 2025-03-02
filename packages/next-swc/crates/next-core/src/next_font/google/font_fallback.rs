use std::collections::HashMap;

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use turbo_binding::{turbo::tasks_fs::FileSystemPathVc, turbopack::core::issue::IssueSeverity};
use turbo_tasks::{
    primitives::{StringVc, StringsVc, U32Vc},
    trace::TraceRawVcs,
};

use super::options::NextFontGoogleOptionsVc;
use crate::{
    next_font::{
        font_fallback::{
            AutomaticFontFallback, FontAdjustment, FontFallback, FontFallbackVc,
            DEFAULT_SANS_SERIF_FONT, DEFAULT_SERIF_FONT,
        },
        issue::NextFontIssue,
        util::{get_scoped_font_family, FontFamilyType},
    },
    util::load_next_json,
};

/// An entry in the Google fonts metrics map
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FontMetricsMapEntry {
    #[allow(unused)]
    family_name: String,
    category: String,
    #[allow(unused)]
    cap_height: i32,
    ascent: i32,
    descent: i32,
    line_gap: u32,
    units_per_em: u32,
    #[allow(unused)]
    x_height: i32,
    x_width_avg: f64,
}

#[derive(Deserialize)]
pub(super) struct FontMetricsMap(pub HashMap<String, FontMetricsMapEntry>);

#[derive(Debug, PartialEq, Serialize, Deserialize, TraceRawVcs)]
struct Fallback {
    pub font_family: String,
    pub adjustment: Option<FontAdjustment>,
}

#[turbo_tasks::function]
pub(super) async fn get_font_fallback(
    context: FileSystemPathVc,
    options_vc: NextFontGoogleOptionsVc,
    request_hash: U32Vc,
) -> Result<FontFallbackVc> {
    let options = options_vc.await?;
    Ok(match &options.fallback {
        Some(fallback) => FontFallback::Manual(StringsVc::cell(fallback.clone())).cell(),
        None => {
            let metrics_json =
                load_next_json(context, "/dist/server/capsize-font-metrics.json").await;
            match metrics_json {
                Ok(metrics_json) => {
                    let fallback = lookup_fallback(
                        &options.font_family,
                        metrics_json,
                        options.adjust_font_fallback,
                    );

                    match fallback {
                        Ok(fallback) => FontFallback::Automatic(
                            AutomaticFontFallback {
                                scoped_font_family: get_scoped_font_family(
                                    FontFamilyType::Fallback.cell(),
                                    options_vc.font_family(),
                                    request_hash,
                                ),
                                local_font_family: StringVc::cell(fallback.font_family),
                                adjustment: fallback.adjustment,
                            }
                            .cell(),
                        )
                        .cell(),
                        Err(_) => {
                            NextFontIssue {
                                path: context,
                                title: StringVc::cell(format!(
                                    "Failed to find font override values for font `{}`",
                                    &options.font_family,
                                )),
                                description: StringVc::cell(
                                    "Skipping generating a fallback font.".to_owned(),
                                ),
                                severity: IssueSeverity::Warning.cell(),
                            }
                            .cell()
                            .as_issue()
                            .emit();
                            FontFallback::Error.cell()
                        }
                    }
                }
                Err(_) => FontFallback::Error.cell(),
            }
        }
    })
}

static FALLBACK_FONT_NAME: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?:^\w|[A-Z]|\b\w)").unwrap());

// From https://github.com/vercel/next.js/blob/1628260b88ce3052ac307a1607b6e8470188ab83/packages/next/src/server/font-utils.ts#L101
fn format_fallback_font_name(font_family: &str) -> String {
    let mut fallback_name = FALLBACK_FONT_NAME
        .replace(font_family, |caps: &regex::Captures| {
            caps.iter()
                .enumerate()
                .map(|(i, font_matches)| {
                    let font_matches = font_matches.unwrap().as_str();
                    if i == 0 {
                        font_matches.to_lowercase()
                    } else {
                        font_matches.to_uppercase()
                    }
                })
                .collect::<Vec<String>>()
                .join("")
        })
        .to_string();
    fallback_name.retain(|c| !c.is_whitespace());
    fallback_name
}

fn lookup_fallback(
    font_family: &str,
    font_metrics_map: FontMetricsMap,
    adjust: bool,
) -> Result<Fallback> {
    let font_family = format_fallback_font_name(font_family);
    let metrics = font_metrics_map
        .0
        .get(&font_family)
        .context("Font not found in metrics")?;

    let fallback = if metrics.category == "serif" {
        &DEFAULT_SERIF_FONT
    } else {
        &DEFAULT_SANS_SERIF_FONT
    };

    let metrics = if adjust {
        // Derived from
        // https://github.com/vercel/next.js/blob/7bfd5829999b1d203e447d30de7e29108c31934a/packages/next/src/server/font-utils.ts#L131
        let main_font_avg_width = metrics.x_width_avg / metrics.units_per_em as f64;
        let fallback_font_avg_width = fallback.x_width_avg / fallback.units_per_em as f64;
        let size_adjust = main_font_avg_width / fallback_font_avg_width;

        let ascent = metrics.ascent as f64 / (metrics.units_per_em as f64 * size_adjust);
        let descent = metrics.descent as f64 / (metrics.units_per_em as f64 * size_adjust);
        let line_gap = metrics.line_gap as f64 / (metrics.units_per_em as f64 * size_adjust);

        Some(FontAdjustment {
            ascent,
            descent,
            line_gap,
            size_adjust,
        })
    } else {
        None
    };

    Ok(Fallback {
        font_family: fallback.name.clone(),
        adjustment: metrics,
    })
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use turbo_binding::turbo::tasks_fs::json::parse_json_with_source_context;

    use super::{FontAdjustment, FontMetricsMap};
    use crate::next_font::google::font_fallback::{lookup_fallback, Fallback};

    #[test]
    fn test_fallback_from_metrics_sans_serif() -> Result<()> {
        let font_metrics: FontMetricsMap = parse_json_with_source_context(
            r#"
            {
                "inter": {
                    "familyName": "Inter",
                    "category": "sans-serif",
                    "capHeight": 2048,
                    "ascent": 2728,
                    "descent": -680,
                    "lineGap": 0,
                    "unitsPerEm": 2816,
                    "xHeight": 1536,
                    "xWidthAvg": 1335
                  }
            }
        "#,
        )?;

        assert_eq!(
            lookup_fallback("Inter", font_metrics, true)?,
            Fallback {
                font_family: "Arial".to_owned(),
                adjustment: Some(FontAdjustment {
                    ascent: 0.9324334770490376,
                    descent: -0.23242476700635833,
                    line_gap: 0.0,
                    size_adjust: 1.0389481114147647
                })
            }
        );
        Ok(())
    }

    #[test]
    fn test_fallback_from_metrics_serif() -> Result<()> {
        let font_metrics: FontMetricsMap = parse_json_with_source_context(
            r#"
            {
                "robotoSlab": {
                    "familyName": "Roboto Slab",
                    "category": "serif",
                    "capHeight": 1456,
                    "ascent": 2146,
                    "descent": -555,
                    "lineGap": 0,
                    "unitsPerEm": 2048,
                    "xHeight": 1082,
                    "xWidthAvg": 969
                  }
            }
        "#,
        )?;

        assert_eq!(
            lookup_fallback("Roboto Slab", font_metrics, true)?,
            Fallback {
                font_family: "Times New Roman".to_owned(),
                adjustment: Some(FontAdjustment {
                    ascent: 0.9239210539440684,
                    descent: -0.23894510015794873,
                    line_gap: 0.0,
                    size_adjust: 1.134135387462914
                })
            }
        );
        Ok(())
    }
}
