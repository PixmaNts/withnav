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


if __name__ == "__main__":
    unittest.main()
