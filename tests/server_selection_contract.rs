//! The real HTTP backend contract consumed by the editor's mocked transport.
use serde_json::{json, Value};
mod common;
use common::server::{spawn_server, TestServerConfig};

async fn command(
    client: &reqwest::Client,
    base: &str,
    id: &str,
    name: &str,
    arguments: &str,
) -> Value {
    let response = client
        .post(format!("{base}/v1/sessions/{id}/commands"))
        .json(&json!({"name":name,"arguments":arguments}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["ok"], true, "{body}");
    body
}

fn identity(selection: &Value, agent: &str, model: &str, source: &str) {
    assert_eq!(selection["agent"], agent);
    assert_eq!(selection["model"], model);
    assert_eq!(selection["gateway"], "mock");
    assert_eq!(
        selection["upstream_id"],
        model.strip_prefix("mock/").unwrap()
    );
    assert_eq!(selection["source"], source);
    assert!(selection.get("variant").is_some());
    assert!(selection.get("variant_source").is_some());
    assert!(selection.get("alias").is_none());
    assert!(selection.get("provider").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_selection_catalog_events_and_assignments_share_canonical_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let state = TestServerConfig::unreachable(tmp.path()).state();
    std::fs::write(tmp.path().join("models.json"), json!({
        "providers":{"mock":{"base_url":"http://127.0.0.1:1/v1"}},
        "models":{
            "mock/combo/base":{"provider":"mock","model":"combo/base","supports_tools":true,"context_window":200000},
            "mock/direct":{"provider":"mock","model":"direct","supports_tools":true,"context_window":200000,
                "variants":{"high":{"reasoning_effort":"high"}},"default_variant":"high"},
            "mock/fill":{"provider":"mock","model":"fill","api":"fim"},
            "mock/typesafe/jev":{"provider":"mock","model":"typesafe/jev","api":"systemone"}
        }
    }).to_string()).unwrap();
    std::fs::write(tmp.path().join("cfg/config.json"), json!({
        "default_agent":"build", "permissions":{"scratch_dirs":[]},
        "agents":{"build":{"model":"mock/combo/base"},"plan":{"model":"mock/combo/base"}},
        "layers":{"reflex":"mock/fill","instinct":"mock/typesafe/jev","metacognition":"mock/combo/base"}
    }).to_string()).unwrap();
    let (addr, task) = spawn_server(state.clone()).await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    let models: Value = client
        .get(format!("{base}/v1/models"))
        .query(&[("workspace", tmp.path())])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entries = models["models"].as_array().unwrap();
    let nested = entries
        .iter()
        .find(|m| m["name"] == "mock/combo/base")
        .unwrap();
    assert_eq!(nested["gateway"], "mock");
    assert_eq!(nested["provider_group"], "combo");
    assert_eq!(nested["upstream_id"], "combo/base");
    assert_eq!(
        entries.iter().find(|m| m["name"] == "mock/direct").unwrap()["provider_group"],
        "mock"
    );
    let created: Value = client
        .post(format!("{base}/v1/sessions"))
        .json(&json!({"workspace":tmp.path(),"editor":true,"agent":"plan"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["session_id"].as_str().unwrap();
    let summary: Value = client
        .get(format!("{base}/v1/sessions/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    identity(&summary, "plan", "mock/combo/base", "config");
    assert_eq!(summary["editor"], true);
    assert!(
        state
            .history()
            .meta(id.parse().unwrap())
            .await
            .unwrap()
            .unwrap()
            .editor
    );
    let switched = command(&client, &base, id, "model", "mock/direct").await;
    identity(
        &switched["data"]["selection"],
        "plan",
        "mock/direct",
        "override",
    );
    assert_eq!(switched["data"]["model"], "mock/direct");
    assert!(switched["data"].get("alias").is_none());
    let events = state
        .history()
        .model_events(id.parse().unwrap())
        .await
        .unwrap();
    let changed = &events.last().unwrap().payload;
    identity(&changed["previous"], "plan", "mock/combo/base", "config");
    identity(&changed["new"], "plan", "mock/direct", "override");
    assert_eq!(changed["operation_id"], switched["operation_id"]);
    let typed: navai::bus::event::ModelChanged = serde_json::from_value(changed.clone()).unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), *changed);
    let variant = command(&client, &base, id, "variant", "").await;
    assert_eq!(variant["data"]["agent"], "plan");
    assert_eq!(variant["data"]["model"], "mock/direct");
    assert_eq!(variant["data"]["offered"], json!(["high"]));
    assert_eq!(variant["data"]["variant_source"], "model_default");
    let bare = command(&client, &base, id, "model", "").await;
    identity(
        &bare["data"]["selection"],
        "plan",
        "mock/direct",
        "override",
    );
    let unchanged = command(&client, &base, id, "model", "mock/direct").await;
    assert_eq!(unchanged["data"]["changed"], false);
    identity(
        &unchanged["data"]["selection"],
        "plan",
        "mock/direct",
        "override",
    );
    let agents: Value = client
        .get(format!("{base}/v1/agents"))
        .query(&[
            ("workspace", tmp.path().to_str().unwrap()),
            ("session_id", id),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let plan = agents["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "plan")
        .unwrap();
    assert_eq!(plan["effective"]["model"], "mock/direct");
    assert_eq!(plan["effective"]["source"], "override");
    assert_eq!(plan["effective"]["variant_source"], "model_default");
    let layers: Value = client
        .get(format!("{base}/v1/layers"))
        .query(&[("workspace", tmp.path())])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(layers["layers"].as_array().unwrap().len(), 3);
    let list: Value = client
        .get(format!("{base}/v1/sessions"))
        .query(&[("workspace", tmp.path())])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    identity(&list["sessions"][0], "plan", "mock/direct", "override");
    assert_eq!(list["sessions"][0]["editor"], true);
    let agent = command(&client, &base, id, "agent", "build").await;
    identity(
        &agent["data"]["selection"],
        "build",
        "mock/combo/base",
        "config",
    );
    let agent = command(&client, &base, id, "agent", "plan").await;
    identity(
        &agent["data"]["selection"],
        "plan",
        "mock/direct",
        "override",
    );
    state.close_session(id.parse().unwrap()).await.unwrap();
    let stopped: Value = client
        .get(format!("{base}/v1/sessions/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    identity(&stopped, "plan", "mock/direct", "override");
    assert_eq!(stopped["editor"], true);
    // Both editor resume entry points send only workspace and resume.
    for _ in 0..2 {
        let resumed = client
            .post(format!("{base}/v1/sessions"))
            .json(&json!({"workspace":tmp.path(),"resume":id}))
            .send()
            .await
            .unwrap();
        assert_eq!(resumed.status(), 200);
        let resumed: Value = resumed.json().await.unwrap();
        assert_eq!(resumed["session_id"], id);
        let summary: Value = client
            .get(format!("{base}/v1/sessions/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            state
                .history()
                .meta(id.parse().unwrap())
                .await
                .unwrap()
                .unwrap()
                .editor
        );
        identity(&summary, "plan", "mock/direct", "override");
        state.close_session(id.parse().unwrap()).await.unwrap();
    }
    let obsolete = client
        .post(format!("{base}/v1/sessions"))
        .json(&json!({"workspace":tmp.path(),"profile":"navigator"}))
        .send()
        .await
        .unwrap();
    assert_eq!(obsolete.status(), 400);
    task.abort();
}
