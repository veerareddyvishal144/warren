// Browser smoke of the Warren dashboard against a REAL dev daemon.
// Usage: npm i --no-save playwright-core && node scripts/dashboard-smoke.mjs
// Needs: a release build (make build), the static gateway, Docker, and a Chrome/Chromium
// binary (CHROME env, default /usr/bin/google-chrome). Starts its own dev daemon on PORT.
import { createRequire } from 'node:module';
import { spawn } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, '..');
const require = createRequire(import.meta.url);
const { chromium } = require('playwright-core');

const PORT = Number(process.env.PORT || 7471);
const CHROME = process.env.CHROME || '/usr/bin/google-chrome';
const WARREND = process.env.WARREN_BIN || path.join(repo, 'target/release/warrend');
const GATEWAY = process.env.WARREN_GATEWAY_BIN || path.join(repo, 'target/x86_64-unknown-linux-musl/release/warren-gateway');
const DATA = process.env.WARREN_SMOKE_DATA || path.join(repo, 'data/dashboard-smoke');
const DASH = `http://warren.localhost:${PORT}`;
const API = `http://api.warren.localhost:${PORT}/v1`;
const daemon = spawn(WARREND, ['--dev', '--listen', `[::]:${PORT}`, '--public-port', String(PORT), '--data-dir', DATA, '--base-domain', 'warren.localhost', '--gateway-bin', GATEWAY, '--log', 'warn'], { stdio: ['ignore', 'inherit', 'inherit'] });
for (let i = 0; i < 100; i++) { try { const r = await fetch(API + '/health'); if (r.ok) break; } catch {} await new Promise(r => setTimeout(r, 200)); }

const problems = [], steps = [];
const browser = await chromium.launch({ headless: true, executablePath: CHROME });
const page = await (await browser.newContext({ viewport: { width: 1280, height: 900 } })).newPage();
page.on('console', m => { if (['error', 'warning'].includes(m.type()) && !m.text().includes('401')) problems.push(`console.${m.type()}: ${m.text()}`); });
page.on('pageerror', e => problems.push('pageerror: ' + e.message));
page.on('requestfailed', r => { if (!r.url().endsWith('/favicon.ico')) problems.push('requestfailed: ' + r.method() + ' ' + r.url()); });
page.on('dialog', d => d.accept());
async function step(name, fn) { try { await fn(); steps.push('ok   ' + name); } catch (e) { steps.push('FAIL ' + name + ': ' + e.message.split('\n')[0]); } }
const text = async sel => (await page.locator(sel).first().textContent()) || '';

await step('login panel loads', async () => { await page.goto(DASH + '/'); await page.locator('#login').waitFor({ state: 'visible', timeout: 15000 }); });
await step('signup through the UI', async () => {
  await page.fill('#signup-name', 'Smoke'); await page.click('#signup-form button[type=submit]');
  await page.locator('#signup-result').waitFor({ state: 'visible', timeout: 15000 });
  const key = await text('#signup-key'); if (!key.startsWith('sk_live_')) throw new Error('no key: ' + key);
  await page.click('#signup-continue'); await page.locator('#main').waitFor({ state: 'visible', timeout: 15000 });
});
let created = false;
await step('create an instance from the form', async () => {
  const newBtn = page.locator('#empty-new-btn, #new-instance-btn, button:has-text("New instance")').first();
  await newBtn.click({ timeout: 10000 });
  await page.locator('#create-form').waitFor({ state: 'visible', timeout: 10000 });
  await page.selectOption('#create-template', 'warren-base');
  await page.fill('#create-name', 'dash-smoke');
  await page.click('#create-submit');
  await page.locator('#detail-panel').waitFor({ state: 'visible', timeout: 20000 });
  created = true;
});
await step('instance reaches running within 120 s', async () => {
  const deadline = Date.now() + 120000;
  while (Date.now() < deadline) { const s = (await text('#detail-status')).trim().toLowerCase(); if (s.includes('running')) return; if (s.includes('failed')) throw new Error('status failed: ' + await text('#detail-reason')); await page.waitForTimeout(1500); }
  throw new Error('still ' + await text('#detail-status'));
});
await step('exec console runs a command', async () => {
  await page.fill('#exec-command', 'echo dash-ok && id -un');
  await page.click('#exec-run');
  await page.locator('#exec-output', { hasText: 'dash-ok' }).waitFor({ timeout: 30000 });
  const out = await text('#exec-output'); if (!out.includes('agent')) throw new Error('unexpected user in output: ' + out.slice(0, 120));
});
await step('billing panel shows the wallet', async () => {
  await page.click('#nav-billing'); await page.locator('#billing-panel').waitFor({ state: 'visible', timeout: 10000 });
  const w = await text('#billing-wallet'); if (!/4\.|5\.|\$/.test(w)) throw new Error('wallet text: ' + w);
  await page.click('#nav-instances');
});
await step('delete the instance from the UI', async () => {
  if (!created) throw new Error('nothing created');
  await page.locator('#instance-list button').first().click();
  await page.click('#act-delete');
  await page.waitForTimeout(3000);
  const r = await fetch(API + '/instances', { headers: { Authorization: 'Bearer ' + await page.evaluate(() => localStorage.getItem('warren.apiKey')) } });
  const left = (await r.json()).data.filter(i => i.status !== 'deleting' && i.status !== 'deleted');
  if (left.length) throw new Error('instances left: ' + left.map(i => i.status).join(','));
});
await page.screenshot({ path: path.join(DATA, 'final.png') }).catch(() => {});
await browser.close(); daemon.kill('SIGTERM');
console.log(steps.join('\n')); console.log('problems: ' + (problems.length ? '\n' + problems.join('\n') : 'none'));
process.exit(steps.some(s => s.startsWith('FAIL')) ? 1 : 0);
