use crate::config::{Config, KeyAction, Layer, LayerKind, StenoConfig};
use crate::keycode::KeyCode;
use crate::steno::engine::resolve_dictionary_path;
use crate::steno::layout::{build_key_map, preset};
use anyhow::Result;
use std::collections::{HashMap, HashSet};

pub fn validate_config(config_path: Option<&std::path::Path>) -> Result<()> {
    use colored::Colorize;

    println!();
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!("  {}", "Config Validation".bright_cyan().bold());
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!();

    let config_path = if let Some(path) = config_path {
        path.to_path_buf()
    } else {
        Config::default_path()?
    };

    println!(
        "  {} {}",
        "Config file:".bright_yellow(),
        config_path.display().to_string().dimmed()
    );
    println!();

    print!("  {} Loading config... ", "→".bright_blue());
    let config = match Config::load_or_hint(&config_path) {
        Ok(cfg) => {
            println!("{}", "✓".bright_green().bold());
            cfg
        }
        Err(e) => {
            println!("{}", "✗".bright_red().bold());
            println!();
            println!("  {} {}", "Error:".bright_red().bold(), e);
            println!();
            return Err(e);
        }
    };

    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    print!("  {} Checking SOCD pairs... ", "→".bright_blue());
    let mut socd_map: HashMap<KeyCode, KeyCode> = HashMap::new();

    let mut extract_socd = |remaps: &HashMap<KeyCode, KeyAction>| {
        let mut pairs = Vec::new();
        for (key, action) in remaps {
            if let KeyAction::SOCD(this_action, opposing_actions) = action {
                if let KeyAction::Key(this_key) = this_action.as_ref() {
                    if key != this_key {
                        warnings.push(format!(
                            "⚠️  SOCD key mismatch: {:?} maps to SOCD({:?}, ...)",
                            key, this_key
                        ));
                    }
                    for opposing_action in opposing_actions {
                        if let KeyAction::Key(opposing_key) = opposing_action.as_ref() {
                            pairs.push((*this_key, *opposing_key));
                        }
                    }
                }
            }
        }
        pairs
    };

    for (key1, key2) in extract_socd(&config.remaps) {
        socd_map.insert(key1, key2);
    }
    for layer_config in config.layers.values() {
        for (key1, key2) in extract_socd(&layer_config.remaps) {
            socd_map.insert(key1, key2);
        }
    }
    for (key1, key2) in extract_socd(&config.game_mode.remaps) {
        socd_map.insert(key1, key2);
    }

    let mut socd_checked = HashSet::new();
    for (key1, key2) in &socd_map {
        if socd_checked.contains(key1) {
            continue;
        }
        if let Some(reverse) = socd_map.get(key2) {
            if reverse != key1 {
                errors.push(format!(
                    "SOCD pair asymmetric: {:?} → {:?}, but {:?} → {:?}",
                    key1, key2, key2, reverse
                ));
            }
            socd_checked.insert(*key1);
            socd_checked.insert(*key2);
        } else {
            errors.push(format!(
                "SOCD missing reverse pair: {:?} → {:?}, but {:?} not defined",
                key1, key2, key2
            ));
        }
    }

    if errors.is_empty() {
        println!("{} {} pairs", "✓".bright_green().bold(), socd_map.len() / 2);
    } else {
        println!("{}", "✗".bright_red().bold());
    }

    print!("  {} Checking timing settings... ", "→".bright_blue());
    if config.tapping_term_ms == 0 || config.tapping_term_ms > 1000 {
        errors.push(format!(
            "tapping_term_ms out of reasonable range (0-1000): {}",
            config.tapping_term_ms
        ));
    }
    let window = config.mt_config.double_tap_window_ms;
    if window == 0 || window > 1000 {
        errors.push(format!(
            "mt_config.double_tap_window_ms out of reasonable range (0-1000): {}",
            window
        ));
    }
    println!("{}", "✓".bright_green().bold());

    print!("  {} Checking layer references... ", "→".bright_blue());
    let mut referenced_layers = HashSet::new();

    let extract_layer_refs = |remaps: &HashMap<KeyCode, KeyAction>| {
        let mut refs = Vec::new();
        for action in remaps.values() {
            if let KeyAction::TO(layer) = action {
                refs.push(layer.0.clone());
            }
        }
        refs
    };

    for layer_name in extract_layer_refs(&config.remaps) {
        referenced_layers.insert(layer_name);
    }
    for layer_config in config.layers.values() {
        for layer_name in extract_layer_refs(&layer_config.remaps) {
            referenced_layers.insert(layer_name);
        }
    }

    let mut missing_layers = Vec::new();
    for layer_name in &referenced_layers {
        if layer_name != "base" && !config.layers.contains_key(&Layer(layer_name.clone())) {
            missing_layers.push(layer_name.clone());
        }
    }

    if missing_layers.is_empty() {
        println!(
            "{} {} layers",
            "✓".bright_green().bold(),
            config.layers.len()
        );
    } else {
        println!("{}", "✗".bright_red().bold());
        for layer_name in missing_layers {
            errors.push(format!("Referenced layer not defined: \"{}\"", layer_name));
        }
    }

    print!("  {} Checking steno layers... ", "→".bright_blue());
    let steno_layers: Vec<(&Layer, &StenoConfig)> = config
        .layers
        .iter()
        .filter_map(|(layer, layer_config)| match &layer_config.kind {
            LayerKind::Steno(steno) => Some((layer, steno)),
            LayerKind::Remap => None,
        })
        .collect();
    let (uid, _) = crate::get_actual_user_uid();
    let home = crate::get_user_home_dir(uid).ok();
    let config_dir = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let mut steno_errors = Vec::new();
    for (layer, steno) in &steno_layers {
        let layout = preset(&steno.layout)
            .map_err(|e| vec![e])
            .and_then(|preset| build_key_map(&preset, &steno.layout_overrides));
        if let Err(layout_errors) = layout {
            for layout_error in layout_errors {
                steno_errors.push(format!("steno layer \"{}\": {}", layer.0, layout_error));
            }
        }
        if steno.translation_timeout_ms.is_some() {
            warnings.push(format!(
                "steno layer \"{}\": translation_timeout_ms is no longer used and can be removed",
                layer.0
            ));
        }
        for raw in &steno.dictionaries {
            let path = resolve_dictionary_path(raw, config_dir, home.as_deref());
            if !path.exists() {
                // Not fatal: the layer loads without this dictionary
                warnings.push(format!(
                    "steno layer \"{}\": dictionary not found: {}",
                    layer.0,
                    path.display()
                ));
            }
        }
    }
    if steno_errors.is_empty() {
        println!(
            "{} {} steno layers",
            "✓".bright_green().bold(),
            steno_layers.len()
        );
    } else {
        println!("{}", "✗".bright_red().bold());
        errors.extend(steno_errors);
    }

    println!();
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );

    if errors.is_empty() && warnings.is_empty() {
        println!(
            "  {} {}",
            "✓".bright_green().bold(),
            "Config is valid!".bright_green()
        );
    } else {
        if !errors.is_empty() {
            println!(
                "  {} {}",
                "✗".bright_red().bold(),
                format!("{} error(s)", errors.len()).bright_red()
            );
            for error in &errors {
                println!("    {} {}", "•".bright_red(), error);
            }
        }
        if !warnings.is_empty() {
            println!(
                "  {} {}",
                "!".bright_yellow().bold(),
                format!("{} warning(s)", warnings.len()).bright_yellow()
            );
            for warning in &warnings {
                println!("    {} {}", "•".bright_yellow(), warning);
            }
        }
    }

    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!();

    if !errors.is_empty() {
        Err(anyhow::anyhow!(
            "Config validation failed with {} error(s)",
            errors.len()
        ))
    } else {
        Ok(())
    }
}
