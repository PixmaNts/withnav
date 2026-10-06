use super::{
    protocol::{ApiKind, ModelEntry},
    state::UiState,
};
use std::collections::BTreeMap;

pub enum Row<'a> {
    Group {
        key: String,
        label: String,
        collapsed: bool,
        /// Visual depth: 0 for a gateway, 1 for a provider under it.
        level: u8,
    },
    Model(&'a ModelEntry),
    /// The "Favorites" heading: always open, never folded.
    Section {
        label: &'static str,
    },
    /// A favorite model listed in the Favorites section, ahead of its place in its group.
    Favorite(&'a ModelEntry),
}

/// Key of the Favorites heading; its prefix tells it from a foldable `group:` row.
pub const SECTION_PREFIX: &str = "section:";

impl Row<'_> {
    pub fn key(&self) -> String {
        match self {
            Self::Group { key, .. } => format!("group:{key}"),
            Self::Model(model) | Self::Favorite(model) => model.name.clone(),
            Self::Section { .. } => format!("{SECTION_PREFIX}favorites"),
        }
    }

    /// Whether this row sits in the Favorites section (as opposed to its model's group).
    pub fn is_favorite_row(&self) -> bool {
        matches!(self, Self::Favorite(_))
    }

    /// Visual depth in the picker: gateway 0, provider 1, model 2, or 1 for a model of a
    /// direct gateway (no provider heading between them).
    pub fn level(&self) -> u8 {
        match self {
            Self::Group { level, .. } => *level,
            Self::Section { .. } => 0,
            Self::Favorite(_) => 1,
            Self::Model(model) if model.provider_group == model.gateway => 1,
            Self::Model(_) => 2,
        }
    }
}

/// Folds every gateway and provider group, or unfolds them all when every group is already
/// folded. Search opens every group, so with a filter there is nothing to do.
pub fn toggle_all(catalog: &[ModelEntry], ui: &mut UiState) {
    if !ui.filter.is_empty() {
        return;
    }
    let any_open = rows(catalog, ui).iter().any(|row| {
        matches!(
            row,
            Row::Group {
                collapsed: false,
                ..
            }
        )
    });
    if !any_open {
        ui.collapsed_models.clear();
        return;
    }
    for model in catalog.iter().filter(|m| m.api == ApiKind::Chat) {
        ui.collapsed_models.insert(model.gateway.clone());
        if model.provider_group != model.gateway {
            ui.collapsed_models
                .insert(format!("{}/{}", model.gateway, model.provider_group));
        }
    }
}

/// The view and keyboard routing share exactly the same rows. Search opens every group. Favorite
/// models come first, in a section that is always open and also shown while searching.
pub fn rows<'a>(catalog: &'a [ModelEntry], ui: &UiState) -> Vec<Row<'a>> {
    let filter = ui.filter.to_lowercase();
    let mut groups: BTreeMap<&str, BTreeMap<&str, Vec<&ModelEntry>>> = BTreeMap::new();
    for model in catalog {
        if model.api == ApiKind::Chat && model.name.to_lowercase().contains(&filter) {
            groups
                .entry(&model.gateway)
                .or_default()
                .entry(&model.provider_group)
                .or_default()
                .push(model);
        }
    }
    let mut rows = vec![];
    let mut favorites: Vec<&ModelEntry> = catalog
        .iter()
        .filter(|m| {
            m.api == ApiKind::Chat
                && ui.favorite_models.contains(&m.name)
                && m.name.to_lowercase().contains(&filter)
        })
        .collect();
    favorites.sort_by(|a, b| a.name.cmp(&b.name));
    if !favorites.is_empty() {
        rows.push(Row::Section { label: "Favorites" });
        rows.extend(favorites.into_iter().map(Row::Favorite));
    }
    for (gateway, providers) in groups {
        let collapsed = filter.is_empty() && ui.collapsed_models.contains(gateway);
        rows.push(Row::Group {
            key: gateway.into(),
            label: gateway.into(),
            collapsed,
            level: 0,
        });
        if collapsed {
            continue;
        }
        for (provider, mut models) in providers {
            let key = format!("{gateway}/{provider}");
            let collapsed = filter.is_empty() && ui.collapsed_models.contains(&key);
            // Direct endpoints have no redundant provider header.
            if provider != gateway {
                rows.push(Row::Group {
                    key,
                    label: provider.into(),
                    collapsed,
                    level: 1,
                });
                if collapsed {
                    continue;
                }
            }
            models.sort_by(|a, b| a.name.cmp(&b.name));
            rows.extend(models.into_iter().map(Row::Model));
        }
    }
    rows
}
