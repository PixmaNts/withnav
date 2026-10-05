use super::{protocol::ModelEntry, state::UiState};
use std::collections::BTreeMap;

pub enum Row<'a> {
    Group {
        key: String,
        label: String,
        collapsed: bool,
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
}

/// The view and keyboard routing share exactly the same rows. Search opens every group.
pub fn rows<'a>(catalog: &'a [ModelEntry], ui: &UiState) -> Vec<Row<'a>> {
    let filter = ui.filter.to_lowercase();
    let mut groups: BTreeMap<&str, BTreeMap<&str, Vec<&ModelEntry>>> = BTreeMap::new();
    for model in catalog {
        if model.name.to_lowercase().contains(&filter) {
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
                    label: format!("  {provider}"),
                    collapsed,
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
