use super::*;
use serde_json::json;
use std::sync::Arc;

fn catalog_of(files: &[(&str, &str)]) -> Catalog {
    let mut catalog = Catalog::default();
    for (origin, json) in files {
        catalog.insert_source(origin, json);
    }
    catalog
}

#[test]
fn a_missing_or_unreadable_value_follows_the_system() {
    assert_eq!(ThemeChoice::from_value(None), ThemeChoice::System);
    assert_eq!(
        ThemeChoice::from_value(Some(&Value::from(3))),
        ThemeChoice::System
    );
    assert_eq!(
        ThemeChoice::from_value(Some(&Value::from("../sepia"))),
        ThemeChoice::System
    );
    assert_eq!(
        ThemeChoice::from_value(Some(&Value::from(""))),
        ThemeChoice::System
    );
}

#[test]
fn a_theme_id_is_kept_and_every_builtin_choice_reads_back() {
    assert_eq!(
        ThemeChoice::from_value(Some(&Value::from("sepia"))),
        ThemeChoice::Named("sepia".to_owned())
    );
    for choice in ThemeChoice::ALL {
        assert_eq!(ThemeChoice::from_value(Some(&choice.to_value())), choice);
    }
    let named = ThemeChoice::Named("ocean".to_owned());
    assert_eq!(ThemeChoice::from_value(Some(&named.to_value())), named);
}

#[test]
fn only_the_system_choice_follows_the_system() {
    assert!(ThemeChoice::System.is_dark(true));
    assert!(!ThemeChoice::System.is_dark(false));
    assert!(!ThemeChoice::Light.is_dark(true));
    assert!(ThemeChoice::Dark.is_dark(false));
}

#[test]
fn bundled_themes_define_every_token_and_match_the_builtins() {
    let catalog = Catalog::bundled();
    assert!(catalog.warnings().is_empty(), "{:?}", catalog.warnings());
    for (id, builtin) in [(DARK_ID, builtin_dark()), (LIGHT_ID, builtin_light())] {
        let file = catalog.get(id).expect("bundled theme");
        for token in Palette::tokens() {
            assert!(
                file.tokens.contains_key(*token),
                "{id} does not set {token}"
            );
        }
        let resolved = catalog.resolve(
            &ThemeChoice::Named(id.to_owned()),
            true,
            &Overrides::default(),
        );
        assert!(!resolved.fell_back, "{:?}", resolved.warnings);
        assert_eq!(resolved.warnings, Vec::<String>::new());
        assert_eq!(resolved.application.as_ref(), builtin);
        assert_eq!(resolved.editor.as_ref(), builtin);
        assert_eq!(resolved.results.as_ref(), builtin);
    }
}

#[test]
fn invalid_json_and_a_future_schema_are_skipped() {
    let catalog = catalog_of(&[
        ("broken.json", "{"),
        (
            "future.json",
            r#"{"schema": 2, "id": "future", "name": "Future"}"#,
        ),
        (
            "ok.json",
            r##"{"schema": 1, "id": "ok", "name": "Ok", "appearance": "dark", "tokens": {"color.text": "#ffffff"}, "extra": true}"##,
        ),
    ]);
    assert!(catalog.get("future").is_none());
    assert!(catalog.get("ok").is_some());
    assert!(
        catalog
            .warnings()
            .iter()
            .any(|warning| warning.contains("invalid JSON"))
    );
    assert!(
        catalog
            .warnings()
            .iter()
            .any(|warning| warning.contains("schema 2"))
    );
    let resolved = catalog.resolve(
        &ThemeChoice::Named("ok".to_owned()),
        true,
        &Overrides::default(),
    );
    assert_eq!(resolved.application.text, 0xffffff);
    assert_eq!(resolved.application.bg, builtin_dark().bg);
}

#[test]
fn a_child_theme_inherits_and_a_missing_token_keeps_the_default() {
    let catalog = catalog_of(&[
        (
            "parent.json",
            r##"{"schema":1,"id":"parent","name":"Parent","appearance":"dark","tokens":{"color.background":"#111111"}}"##,
        ),
        (
            "child.json",
            r#"{"schema":1,"id":"child","name":"Child","extends":"parent","tokens":{"color.text":"{color.background}"}}"#,
        ),
    ]);
    let resolved = catalog.resolve(
        &ThemeChoice::Named("child".to_owned()),
        true,
        &Overrides::default(),
    );
    assert!(!resolved.fell_back);
    assert_eq!(resolved.application.bg, 0x111111);
    assert_eq!(resolved.application.text, 0x111111);
    assert_eq!(resolved.application.mint, builtin_dark().mint);
    assert_eq!(resolved.appearance, Appearance::Dark);
}

#[test]
fn inherited_aliases_follow_child_and_user_overrides_on_each_surface() {
    let mut catalog = Catalog::bundled();
    catalog.insert_source("ocean.json", r##"{"schema":1,"id":"ocean","name":"Ocean","extends":"jykre-dark","tokens":{"color.accent":"#2266aa"},"editor":{"color.accent":"#123456"}}"##);
    let choice = ThemeChoice::Named("ocean".to_owned());
    let resolved = catalog.resolve(&choice, false, &Overrides::default());
    assert_eq!(resolved.application.button_primary_bg, 0x2266aa);
    assert_eq!(resolved.application.chart_fill, 0x2266aa);
    assert_eq!(resolved.application.state_success, 0x2266aa);
    assert_eq!(resolved.editor.sql_string, 0x123456);
    assert_eq!(resolved.results.sql_string, 0x2266aa);

    let overrides = Overrides::from_value(Some(&json!({
        "tokens": {"color.accent":"#abcdef"},
        "results": {"value.number":"{color.accent}", "color.accent":"#654321"}
    })));
    let resolved = catalog.resolve(&choice, false, &overrides);
    assert_eq!(resolved.application.button_primary_bg, 0xabcdef);
    assert_eq!(resolved.editor.sql_string, 0x123456);
    assert_eq!(resolved.results.value_number, 0x654321);
    assert_eq!(resolved.results.button_primary_bg, 0x654321);
    assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
}

#[test]
fn a_literal_replaces_an_inherited_alias_and_invalid_values_keep_it() {
    let mut catalog = Catalog::bundled();
    catalog.insert_source("child.json", r##"{"schema":1,"id":"child","name":"Child","extends":"jykre-dark","tokens":{"color.accent":"#123456","button.primary.background":"#654321","state.success":"invalid"}}"##);
    let overrides = Overrides::from_value(Some(&json!({"tokens":{"color.accent":"#abcdef"}})));
    let resolved = catalog.resolve(&ThemeChoice::Named("child".to_owned()), true, &overrides);
    assert_eq!(resolved.application.button_primary_bg, 0x654321);
    assert_eq!(resolved.application.state_success, 0xabcdef);
    assert!(
        resolved
            .warnings
            .iter()
            .any(|warning| warning.contains("invalid colour"))
    );
}

#[test]
fn colours_and_aliases_convert_between_rgb_and_rgba_tokens() {
    for (literal, rgb, rgba) in [
        ("#123", 0x112233, 0x112233ff),
        ("#123456", 0x123456, 0x123456ff),
        ("#12345678", 0x123456, 0x12345678),
        ("#00000000", 0, 0),
    ] {
        let overrides = Overrides::from_value(Some(&json!({"tokens":{
            "color.text": literal,
            "color.scrim": literal,
            "search.match.background": "{color.text}",
            "color.accent": "{color.scrim}",
            "scrollbar.thumb": "{color.scrim}"
        }})));
        let resolved = Catalog::bundled().resolve(&ThemeChoice::Dark, true, &overrides);
        assert_eq!(resolved.application.text, rgb, "{literal}");
        assert_eq!(resolved.application.scrim, rgba, "{literal}");
        assert_eq!(
            resolved.application.match_bg,
            (rgb << 8) | 0xff,
            "{literal}"
        );
        assert_eq!(resolved.application.mint, rgb, "{literal}");
        assert_eq!(resolved.application.scrollbar_thumb, rgba, "{literal}");
        assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
    }
}

#[test]
fn inheritance_allows_eight_themes_and_rejects_the_ninth_or_a_missing_parent() {
    let mut catalog = Catalog::default();
    for index in 0..9 {
        let mut file =
            json!({"schema":1,"id":format!("theme-{index}"),"name":"Theme","appearance":"light"});
        if index > 0 {
            file["extends"] = json!(format!("theme-{}", index - 1));
        }
        catalog.insert_source("fixture", &file.to_string());
    }
    let eight = catalog.resolve(
        &ThemeChoice::Named("theme-7".to_owned()),
        true,
        &Overrides::default(),
    );
    assert!(!eight.fell_back);
    assert_eq!(eight.appearance, Appearance::Light);
    let nine = catalog.resolve(
        &ThemeChoice::Named("theme-8".to_owned()),
        true,
        &Overrides::default(),
    );
    assert!(nine.fell_back);
    assert_eq!(nine.appearance, Appearance::Dark);
    catalog.insert_source(
        "missing.json",
        r#"{"schema":1,"id":"missing","name":"Missing","extends":"absent"}"#,
    );
    assert!(
        catalog
            .resolve(
                &ThemeChoice::Named("missing".to_owned()),
                false,
                &Overrides::default()
            )
            .fell_back
    );
}

#[test]
fn colour_parser_rejects_non_hex_and_unsupported_lengths() {
    for value in [
        "",
        "123456",
        "#12",
        "#1234",
        "#12345",
        "#1234567",
        "#123456789",
        "#gggggg",
        "#ééé",
    ] {
        assert_eq!(parse_color(value), None, "{value}");
    }
}

#[test]
fn file_ids_cannot_collide_with_persisted_builtin_choices() {
    let mut catalog = Catalog::bundled();
    for id in ["system", "light", "dark"] {
        catalog.insert_source(
            "reserved.json",
            &json!({"schema":1,"id":id,"name":"Reserved"}).to_string(),
        );
        assert!(catalog.get(id).is_none());
        assert!(!catalog.extra_ids().contains(&id));
    }
    assert_eq!(catalog.warnings().len(), 3);
}

#[test]
fn editor_and_results_sections_inherit_from_the_parent() {
    let catalog = catalog_of(&[
        (
            "parent.json",
            r##"{"schema":1,"id":"parent","name":"Parent","appearance":"dark","editor":{"editor.caret":"#111111"},"results":{"grid.header":"#222222"}}"##,
        ),
        (
            "child.json",
            r##"{"schema":1,"id":"child","name":"Child","extends":"parent","editor":{"editor.background":"#333333"}}"##,
        ),
    ]);
    let resolved = catalog.resolve(
        &ThemeChoice::Named("child".to_owned()),
        true,
        &Overrides::default(),
    );
    assert!(!resolved.fell_back, "{:?}", resolved.warnings);
    assert_eq!(resolved.editor.editor_caret, 0x111111);
    assert_eq!(resolved.editor.editor_bg, 0x333333);
    assert_eq!(resolved.results.grid_header, 0x222222);
    assert_eq!(
        resolved.application.editor_caret,
        builtin_dark().editor_caret
    );
    assert_eq!(resolved.application.editor_bg, builtin_dark().editor_bg);
}

#[test]
fn a_cycle_falls_back_and_an_alias_cycle_keeps_the_default() {
    let catalog = catalog_of(&[
        (
            "a.json",
            r#"{"schema":1,"id":"a","name":"A","extends":"b","appearance":"light","tokens":{}}"#,
        ),
        (
            "b.json",
            r#"{"schema":1,"id":"b","name":"B","extends":"a","appearance":"light","tokens":{}}"#,
        ),
    ]);
    let resolved = catalog.resolve(
        &ThemeChoice::Named("a".to_owned()),
        true,
        &Overrides::default(),
    );
    assert!(resolved.fell_back);
    assert_eq!(resolved.application.bg, builtin_dark().bg);
    assert!(
        resolved
            .warnings
            .iter()
            .any(|warning| warning.contains("circular"))
    );

    let catalog = catalog_of(&[(
        "loop.json",
        r#"{"schema":1,"id":"loop","name":"Loop","appearance":"dark","tokens":{"color.text":"{color.accent}","color.accent":"{color.text}"}}"#,
    )]);
    let resolved = catalog.resolve(
        &ThemeChoice::Named("loop".to_owned()),
        false,
        &Overrides::default(),
    );
    assert!(!resolved.fell_back);
    assert_eq!(resolved.application.text, builtin_dark().text);
    assert_eq!(resolved.application.mint, builtin_dark().mint);
    assert!(
        resolved
            .warnings
            .iter()
            .any(|warning| warning.contains("circular reference"))
    );
}

#[test]
fn invalid_colours_are_skipped_and_user_overrides_win() {
    let catalog = catalog_of(&[(
        "base.json",
        r##"{"schema":1,"id":"base","name":"Base","appearance":"dark","tokens":{"color.text":"not-a-colour","color.accent":"#112233"}}"##,
    )]);
    let overrides = Overrides::from_value(Some(&json!({
        "tokens": { "color.accent": "#abcdef" },
        "editor": { "sql.keyword": "{color.accent}" },
        "results": { "grid.header": "#010203" }
    })));
    let resolved = catalog.resolve(&ThemeChoice::Named("base".to_owned()), true, &overrides);
    assert_eq!(resolved.application.text, builtin_dark().text);
    assert_eq!(resolved.application.mint, 0xabcdef);
    assert_eq!(resolved.editor.sql_keyword, 0xabcdef);
    assert_eq!(resolved.application.sql_keyword, builtin_dark().sql_keyword);
    assert_eq!(resolved.results.grid_header, 0x010203);
    assert_eq!(resolved.application.grid_header, builtin_dark().grid_header);
    assert!(
        resolved
            .warnings
            .iter()
            .any(|warning| warning.contains("invalid colour"))
    );
}

#[test]
fn switching_themes_replaces_the_one_in_memory() {
    let catalog = Catalog::bundled();
    let dark = catalog.resolve(&ThemeChoice::Dark, false, &Overrides::default());
    install(Arc::new(dark));
    assert_eq!(active().application.bg, builtin_dark().bg);
    let light = catalog.resolve(&ThemeChoice::Light, true, &Overrides::default());
    install(Arc::new(light));
    assert_eq!(active().application.bg, builtin_light().bg);
    assert_eq!(active().appearance, Appearance::Light);
    let missing = catalog.resolve(
        &ThemeChoice::Named("no-such-theme".to_owned()),
        true,
        &Overrides::default(),
    );
    assert!(missing.fell_back);
    install(Arc::new(missing));
    assert_eq!(active().application.bg, builtin_dark().bg);
}

#[test]
fn a_dropped_file_is_loaded_from_a_directory_without_recompiling() {
    let root =
        std::env::temp_dir().join(format!("jykre-themes-{}", jykre_core::ConnectionId::new()));
    let themes = root.join("themes");
    std::fs::create_dir_all(&themes).unwrap();
    std::fs::write(
        themes.join("ocean.json"),
        r##"{"schema":1,"id":"ocean","name":"Ocean","appearance":"dark","extends":"jykre-dark","tokens":{"color.accent":"#2266aa"}}"##,
    )
    .unwrap();
    let catalog = load_catalog(&theme_directories(&root, None));
    let resolved = catalog.resolve(
        &ThemeChoice::Named("ocean".to_owned()),
        true,
        &Overrides::default(),
    );
    assert!(!resolved.fell_back, "{:?}", resolved.warnings);
    assert_eq!(resolved.application.mint, 0x2266aa);
    assert_eq!(resolved.application.bg, builtin_dark().bg);
    assert!(catalog.extra_ids().contains(&"ocean"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn theme_directories_have_stable_precedence_and_oversized_files_are_skipped() {
    let root = std::env::temp_dir().join(format!(
        "jykre-theme-order-{}",
        jykre_core::ConnectionId::new()
    ));
    let first = root.join("first");
    let second = root.join("second");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    for (dir, name, color) in [
        (&first, "a.json", "#111111"),
        (&first, "z.json", "#222222"),
        (&second, "a.json", "#333333"),
    ] {
        let file = json!({"schema":1,"id":"same","name":"Same","tokens":{"color.text":color}});
        std::fs::write(dir.join(name), file.to_string()).unwrap();
    }
    std::fs::write(second.join("large.json"), " ".repeat(1024 * 1024 + 1)).unwrap();
    let catalog = Catalog::load(std::slice::from_ref(&first));
    assert_eq!(
        catalog
            .resolve(
                &ThemeChoice::Named("same".to_owned()),
                true,
                &Overrides::default()
            )
            .application
            .text,
        0x222222
    );
    let catalog = Catalog::load(&[first, second]);
    assert_eq!(
        catalog
            .resolve(
                &ThemeChoice::Named("same".to_owned()),
                true,
                &Overrides::default()
            )
            .application
            .text,
        0x333333
    );
    assert!(
        catalog
            .warnings()
            .iter()
            .any(|warning| warning.contains("exceeds 1 MiB"))
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn the_choice_and_the_overrides_survive_in_the_settings_store() {
    let dir = std::env::temp_dir().join(format!(
        "jykre-theme-pref-{}",
        jykre_core::ConnectionId::new()
    ));
    let repository = jykre_storage::ConfigRepository::open(&dir).await.unwrap();
    let choice = ThemeChoice::Named("ocean".to_owned());
    repository
        .set_setting(SETTINGS_KEY, &choice.to_value())
        .await
        .unwrap();
    let overrides = Overrides::from_value(Some(&json!({
        "tokens": { "color.text": "#010203" },
        "results": { "value.null": "#040506" }
    })));
    repository
        .set_setting(OVERRIDES_KEY, &overrides.to_value())
        .await
        .unwrap();
    repository.close().await;

    let reopened = jykre_storage::ConfigRepository::open(&dir).await.unwrap();
    let stored = reopened.setting(SETTINGS_KEY).await.unwrap();
    let stored_overrides = reopened.setting(OVERRIDES_KEY).await.unwrap();
    reopened.close().await;
    assert_eq!(ThemeChoice::from_value(stored.as_ref()), choice);
    let overrides = Overrides::from_value(stored_overrides.as_ref());
    let resolved = Catalog::bundled().resolve(&choice, true, &overrides);
    assert!(resolved.fell_back);
    assert_eq!(resolved.application.text, 0x010203);
    assert_eq!(resolved.results.value_null, 0x040506);
    assert_eq!(resolved.application.value_null, builtin_dark().value_null);
    let _ = std::fs::remove_dir_all(&dir);
}
