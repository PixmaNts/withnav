import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "import-opencode-models"
loader = importlib.machinery.SourceFileLoader("import_catalog", str(SCRIPT))
spec = importlib.util.spec_from_loader(loader.name, loader)
importer = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = importer
loader.exec_module(importer)


def source(key="{env:TEST_KEY}"):
    return {"provider": {gateway: {
        "npm": "@ai-sdk/openai-compatible",
        "options": {"baseURL": "http://localhost:9999/v1", "apiKey": key},
        "models": {"combo/gpt-6-astra": {}},
    } for gateway in ("omniroute", "other")}}


class CatalogTests(unittest.TestCase):
    def converted(self):
        return importer.convert(source(), False, lambda _: None)[:2]

    def test_qualified_names_and_no_roles(self):
        providers, models = self.converted()
        self.assertEqual(set(models), {"omniroute/combo/gpt-6-astra",
                                      "other/combo/gpt-6-astra"})
        self.assertEqual(models["other/combo/gpt-6-astra"]["model"], "combo/gpt-6-astra")
        self.assertEqual(models["other/combo/gpt-6-astra"]["provider"], "other")

    def test_duplicate_json_keys(self):
        for text in ('{"provider":{"omniroute":{},"omniroute":{}}}',
                     '{"models":{"m":{},"m":{}}}'):
            with self.assertRaisesRegex(importer.ImportError_, "duplicate JSON"):
                importer.load_json(text, jsonc=True)

    def test_legacy_equivalent_aliases_and_conflict(self):
        providers, models = self.converted()
        entry = {"provider": "omniroute", "model": "m"}
        old = {"models": {"default": entry, "fast": dict(entry)}}
        _, merged = importer.merge_catalog(providers, models, old)
        self.assertIn("omniroute/m", merged)
        self.assertNotIn("default", merged)
        old["models"]["fast"]["supports_tools"] = False
        with self.assertRaisesRegex(importer.ImportError_, "conflicting metadata.*omniroute/m"):
            importer.merge_catalog(providers, models, old)

    def test_manual_api_limits_unmanaged_provider(self):
        providers, models = self.converted()
        jev = {"provider": "omniroute", "model": "typesafe/jev-1.13",
               "api": "systemone", "context_window": 64000, "max_output_tokens": 2048}
        mercury = {"provider": "inception", "model": "mercury-edit-2",
                   "api": "fim", "context_window": 128000, "max_output_tokens": 512}
        old = {"providers": {"inception": {"base_url": "https://example.test/v1",
                                          "api_key_env": "FIM_KEY"}},
               "models": {"instinct": jev, "reflex": mercury}}
        providers, models = importer.merge_catalog(providers, models, old)
        self.assertEqual(models["omniroute/typesafe/jev-1.13"], jev)
        self.assertEqual(models["inception/mercury-edit-2"], mercury)
        self.assertEqual(providers["inception"], old["providers"]["inception"])

    def test_combo_variants_manual_precedence(self):
        providers, models = self.converted()
        name = "omniroute/combo/gpt-6-astra"
        self.assertEqual(models[name]["variants"], {
            tier: {"reasoning_effort": tier} for tier in ("low", "medium", "high")})
        old = {"models": {name: {"provider": "omniroute", "model": "combo/gpt-6-astra",
                                "default_variant": "custom", "variants": {
                                    "low": {"reasoning_effort": "none"},
                                    "custom": {"temperature": 0.2}}}}}
        _, models = importer.merge_catalog(providers, models, old)
        self.assertEqual(models[name]["default_variant"], "custom")
        self.assertEqual(models[name]["variants"]["low"], {"reasoning_effort": "none"})
        self.assertIn("custom", models[name]["variants"])
        self.assertNotIn("max", models[name]["variants"])
        self.assertNotIn("xhigh", models[name]["variants"])

    def cli(self, directory, *args):
        return subprocess.run([sys.executable, str(SCRIPT), "--config-dir", str(directory),
                               "--opencode", str(directory / "source.jsonc"), *args],
                              capture_output=True, text=True,
                              env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"})

    def test_cli_dry_run_credentials_and_config_unchanged(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            secret = "synthetic-secret-never-log"
            (d / "source.jsonc").write_text(json.dumps(source(secret)))
            (d / "config.json").write_bytes(b'{"layers":{"reflex":"manual"}}\n')
            (d / "auth.json").write_text('{"unmanaged":{"api_key":"synthetic-old"}}')
            before = {p.name: p.read_bytes() for p in d.iterdir()}
            result = self.cli(d, "--dry-run")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(before, {p.name: p.read_bytes() for p in d.iterdir()})
            result = self.cli(d)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn(secret, result.stdout + result.stderr + (d / "models.json").read_text())
            self.assertEqual((d / "config.json").read_bytes(), before["config.json"])
            self.assertEqual((d / "auth.json").stat().st_mode & 0o777, 0o600)
            (d / "source.jsonc").write_text(json.dumps(source()))
            self.assertEqual(self.cli(d).returncode, 0)
            self.assertEqual(json.loads((d / "auth.json").read_text()),
                             {"unmanaged": {"api_key": "synthetic-old"}})

    def test_keyless_and_credential_url_protection(self):
        _, _, keys, env = importer.convert(source(""), False, lambda _: None)
        self.assertEqual((keys, env), ({}, set()))
        for url in ("http://user:password@localhost/v1", "http://localhost/v1?api_key=secret"):
            with self.assertRaises(importer.ImportError_):
                importer.check_base_url("omniroute", url)

    def test_cli_conflict_changes_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "source.jsonc").write_text(json.dumps(source()))
            (d / "models.json").write_text(json.dumps({"models": {
                "default": {"provider": "omniroute", "model": "m"},
                "fast": {"provider": "omniroute", "model": "m", "api": "fim"}}}))
            before = {p.name: p.read_bytes() for p in d.iterdir()}
            result = self.cli(d)
            self.assertEqual(result.returncode, 1)
            self.assertIn("conflicting metadata", result.stderr)
            self.assertEqual(before, {p.name: p.read_bytes() for p in d.iterdir()})

    def test_identity_change_requires_explicit_credential(self):
        for credential in (None, "", "{env:NEW_KEY}", "synthetic-new-key"):
            with self.subTest(credential_kind=bool(credential)), tempfile.TemporaryDirectory() as tmp:
                d = Path(tmp)
                oc = source()
                if credential is None:
                    del oc["provider"]["omniroute"]["options"]["apiKey"]
                else:
                    oc["provider"]["omniroute"]["options"]["apiKey"] = credential
                (d / "source.jsonc").write_text(json.dumps(oc))
                (d / "models.json").write_text(json.dumps({"providers": {
                    "omniroute": {"base_url": "http://localhost:9999/old-path",
                                   "api_key_env": "TEST_KEY"}}}))
                (d / "auth.json").write_text(json.dumps({
                    "omniroute": {"api_key": "synthetic-old-key"},
                    "unrelated": {"api_key": "synthetic-unrelated"}}))
                before = {p.name: p.read_bytes() for p in d.iterdir()}
                result = self.cli(d)
                if not credential:
                    self.assertEqual(result.returncode, 1)
                    self.assertEqual(before, {p.name: p.read_bytes() for p in d.iterdir()})
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    auth = json.loads((d / "auth.json").read_text())
                    self.assertEqual(auth["unrelated"], {"api_key": "synthetic-unrelated"})
                    if credential.startswith("{env:"):
                        self.assertNotIn("omniroute", auth)
                    else:
                        self.assertEqual(auth["omniroute"], {"api_key": credential})

    def test_secret_errors_are_value_free_and_write_free(self):
        secret = "synthetic-secret"
        bad_providers = ({"base_url": "http://localhost/v1", "api_key": secret},
                         {"base_url": f"http://user:{secret}@localhost/v1"},
                         {"base_url": "http://localhost/v1", "headers": {"Authorization": secret}})
        for bad in (*bad_providers, None):
            with self.subTest(case=bad is None), tempfile.TemporaryDirectory() as tmp:
                d = Path(tmp)
                oc = source(secret + "{file:x}" if bad is None else "{env:TEST_KEY}")
                (d / "source.jsonc").write_text(json.dumps(oc))
                if bad is not None:
                    (d / "models.json").write_text(json.dumps({"providers": {"retained": bad}}))
                before = {p.name: p.read_bytes() for p in d.iterdir()}
                result = self.cli(d)
                self.assertEqual(result.returncode, 1)
                self.assertNotIn(secret, result.stdout + result.stderr)
                self.assertEqual(before, {p.name: p.read_bytes() for p in d.iterdir()})

    def test_refresh_limits_capabilities_and_idempotence(self):
        oc = source()
        mid = "combo/gpt-6-astra"
        name = "omniroute/" + mid
        oc["provider"]["omniroute"]["models"][mid] = {
            "limit": {"context": 32000, "output": 1000}, "supports_tools": True, "api": "chat"}
        p, m, _, _ = importer.convert(oc, True, lambda _: None)
        m[name]["variants"]["custom"] = {"reasoning_effort": "low"}
        m[name]["default_variant"] = "custom"
        old = {"providers": p, "models": m}
        oc["provider"]["omniroute"]["models"][mid].update({
            "limit": {"context": 64000, "output": 2000}, "supports_tools": False,
            "supports_parallel_tool_calls": False, "api": "fim"})
        p2, m2, _, _ = importer.convert(oc, True, lambda _: None)
        p2, m2 = importer.merge_catalog(p2, m2, old)
        self.assertEqual(m2[name]["context_window"], 64000)
        self.assertEqual(m2[name]["max_output_tokens"], 2000)
        self.assertFalse(m2[name]["supports_tools"])
        self.assertFalse(m2[name]["supports_parallel_tool_calls"])
        self.assertEqual(m2[name]["api"], "fim")
        self.assertEqual(m2[name]["default_variant"], "custom")
        self.assertIn("custom", m2[name]["variants"])
        fresh_p, fresh_m, _, _ = importer.convert(oc, True, lambda _: None)
        self.assertEqual(importer.merge_catalog(fresh_p, fresh_m, {"providers": p2, "models": m2}),
                         (p2, m2))
        fresh_p, fresh_m, _, _ = importer.convert(oc, False, lambda _: None)
        _, merged = importer.merge_catalog(fresh_p, fresh_m, old)
        self.assertEqual(merged[name]["context_window"], 32000)

    def test_comment_cannot_join_tokens(self):
        with self.assertRaises(ValueError):
            importer.load_json('{"value":1/*comment*/2}', jsonc=True)
        self.assertEqual(importer.load_json('{"value":1/*comment*/}', jsonc=True), {"value": 1})

    def test_source_matching_dedicated_api_limits_stay_manual(self):
        oc = source()
        models = oc["provider"]["omniroute"]["models"]
        models["typesafe/jev-1.13"] = {"limit": {"context": 90000, "output": 9000}}
        models["mercury-edit-2"] = {"limit": {"context": 90000, "output": 9000}}
        p, m, _, _ = importer.convert(oc, True, lambda _: None)
        old = {"models": {mid: {"provider": "omniroute", "model": mid, "api": api,
                                 "context_window": window, "max_output_tokens": output}
                          for mid, api, window, output in (
                              ("typesafe/jev-1.13", "systemone", 64000, 2048),
                              ("mercury-edit-2", "fim", 128000, 512))}}
        _, merged = importer.merge_catalog(p, m, old)
        for mid, entry in old["models"].items():
            self.assertEqual(merged["omniroute/" + mid]["context_window"], entry["context_window"])
            self.assertEqual(merged["omniroute/" + mid]["max_output_tokens"], entry["max_output_tokens"])

    def test_provider_identity_full_url_and_routing(self):
        base = {"base_url": "http://localhost:9999/v1", "api_key_env": "TEST_KEY"}
        for field, value in (("base_url", "https://localhost:9999/v1"),
                             ("base_url", "http://localhost:9998/v1"),
                             ("base_url", "http://otherhost:9999/v1"),
                             ("headers", {"X-Destination": "other"}),
                             ("api_key_env", "OTHER_KEY")):
            with self.assertRaises(importer.ImportError_):
                importer.check_provider_changes({"omniroute": {**base, field: value}},
                                                {"providers": {"omniroute": base}}, {}, set())
        importer.check_provider_changes({"omniroute": base}, {"providers": {
            "omniroute": {**base, "base_url": base["base_url"] + "/"}}}, {}, set())


if __name__ == "__main__":
    unittest.main()
