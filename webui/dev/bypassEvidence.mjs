// Manual evidence runner; uses the existing scratch Playwright installation.
import { chromium } from '/tmp/opencode/pw/node_modules/playwright/index.mjs';
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
const url = `http://127.0.0.1:5173/#/chat?ws=${encodeURIComponent(process.cwd())}&s=${process.env.SESSION_ID}`;
await page.goto(url);
await page.locator('.bypass-toggle:not(:disabled)').waitFor();
for (const theme of ['ayu-dark', 'ayu-light', 'nord-dark', 'nord-light']) {
  await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
  if (await page.locator('.bypass-toggle').getAttribute('aria-pressed') === 'true') await page.locator('.bypass-toggle').click();
  await page.waitForFunction(() => document.querySelector('.bypass-toggle')?.getAttribute('aria-pressed') === 'false' && !document.querySelector('.bypass-toggle')?.disabled);
  await page.screenshot({ path: `/tmp/opencode/v061-bypass/webui-${theme}-off.png` });
  await page.locator('.bypass-toggle').click();
  await page.waitForFunction(() => document.querySelector('.bypass-toggle')?.getAttribute('aria-pressed') === 'true');
  await page.locator('.sess .bypass-badge').waitFor();
  await page.getByText(/Bypass ON · user:http/).first().waitFor();
  await page.screenshot({ path: `/tmp/opencode/v061-bypass/webui-${theme}-on.png` });
}
await browser.close();
