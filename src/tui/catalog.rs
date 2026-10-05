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
}

impl Row<'_> {
    pub fn key(&self) -> String {
        match self {
            Self::Group { key, .. } => format!("group:{key}"),
            Self::Model(model) => model.name.clone(),
        }
    }

    /// Visual depth in the picker: gateway 0, provider 1, model 2, or 1 for a model of a
    /// direct gateway (no provider heading between them).
    pub fn level(&self) -> u8 {
        match self {
            Self::Group { level, .. } => *level,
            Self::Model(model) if model.provider_group == model.gateway => 1,
            Self::Model(_) => 2,
        }
    }
}

/// The view and keyboard routing share exactly the same rows. Search opens every group.
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
