import assert from "node:assert/strict";

import { chromium } from "@playwright/test";
import type { Page } from "@playwright/test";

import { startHarness, type Harness } from "./harness.ts";

function launchBrowser() {
  return chromium.launch({
    executablePath: process.env.CHROME_PATH || "/usr/bin/chromium",
    headless: true,
    args: ["--no-sandbox"],
  });
}

async function withBrowserSession(run: (context: { harness: Harness; page: Page }) => Promise<void>) {
  const harness = await startHarness();
  const browser = await launchBrowser();
  const page = await browser.newPage();

  try {
    await run({ harness, page });
  } finally {
    await browser.close();
    await harness.close();
  }
}

async function verifyModelPicker() {
  await withBrowserSession(async ({ harness, page }) => {
    await page.goto(harness.baseUrl, { waitUntil: "load" });
    await page.waitForFunction(
      () => document.querySelector('[aria-label="Model"]')?.textContent?.includes("fake/fake-model"),
    );
    await page.getByText("What should mu help with next?").waitFor({ state: "visible" });
    assert.equal(
      (await page.getByRole("button", { name: "Model" }).textContent())?.trim(),
      "fake/fake-model",
    );
  });
}

async function verifyPromptSubmission() {
  await withBrowserSession(async ({ harness, page }) => {
    await page.goto(harness.baseUrl, { waitUntil: "load" });

    const prompt = "hello from browser";
    await page.getByRole("textbox", { name: "Prompt" }).fill(prompt);
    await page.getByRole("button", { name: "Send prompt" }).click();

    await page
      .locator(".markdown-rendered")
      .filter({ hasText: `Fake response to: ${prompt}` })
      .waitFor({ state: "visible" });

    assert.equal(
      harness.providerRequests.some((request) => JSON.stringify(request).includes(prompt)),
      true,
    );
  });
}

async function verifyFlatStreamingToolTranscript() {
  await withBrowserSession(async ({ harness, page }) => {
    await page.goto(harness.baseUrl, { waitUntil: "load" });

    await page.getByRole("textbox", { name: "Prompt" }).fill("please stream a tool");
    await page.getByRole("button", { name: "Send prompt" }).click();

    await page.locator(".conversation-card").waitFor({ state: "detached" });
    await page
      .locator(".markdown-rendered")
      .filter({ hasText: "I'll run a quick readonly check." })
      .waitFor({ state: "visible" });

    const tool = page.locator(".tool-part").filter({ hasText: "Stream fixture output" });
    await tool.waitFor({ state: "visible" });
    await tool.locator(".tool-part-details:not([open])").waitFor({ state: "attached" });
    await tool.locator(".tool-risk", { hasText: "readonly" }).waitFor({ state: "visible" });

    await tool.locator(".tool-part-summary").click();
    const scriptDisclosure = tool.locator(".tool-disclosure").filter({ hasText: "Command" });
    const outputDisclosure = tool.locator(".tool-disclosure").filter({ hasText: "Output" });
    await scriptDisclosure.locator("summary").click();
    await scriptDisclosure
      .locator(".tool-disclosure-body")
      .filter({ hasText: 'printf "tool-line-$item\\n"' })
      .waitFor({ state: "visible" });

    await outputDisclosure.locator("summary").click();
    await outputDisclosure
      .locator(".tool-disclosure-body")
      .filter({ hasText: "tool-line-1" })
      .waitFor({ state: "visible" });
    await outputDisclosure
      .locator(".tool-disclosure-body")
      .filter({ hasText: "tool-line-4" })
      .waitFor({ state: "visible" });
    await outputDisclosure.locator(".tool-disclosure-body").waitFor({ state: "visible" });

    await page
      .locator(".markdown-rendered")
      .filter({ hasText: "Tool output received." })
      .waitFor({ state: "visible" });
    await tool.locator(".tool-part-details[open]").waitFor({ state: "attached" });
    assert.equal(await outputDisclosure.evaluate((node) => node.hasAttribute("open")), true);
    assert.equal(await page.locator(".conversation-card").count(), 0);
  });
}

async function verifyConcurrentToolOutputAttribution() {
  await withBrowserSession(async ({ harness, page }) => {
    await page.goto(harness.baseUrl, { waitUntil: "load" });

    await page.getByRole("textbox", { name: "Prompt" }).fill("please stream concurrent tools");
    await page.getByRole("button", { name: "Send prompt" }).click();

    await page
      .locator(".markdown-rendered")
      .filter({ hasText: "I'll run two readonly checks at once." })
      .waitFor({ state: "visible" });

    const slowTool = page.locator(".tool-part").filter({ hasText: "Slow concurrent fixture" });
    const fastTool = page.locator(".tool-part").filter({ hasText: "Fast concurrent fixture" });
    await slowTool.waitFor({ state: "visible" });
    await fastTool.waitFor({ state: "visible" });

    await slowTool.locator(".tool-part-summary").click();
    await fastTool.locator(".tool-part-summary").click();

    const slowOutput = slowTool.locator(".tool-disclosure").filter({ hasText: "Output" });
    const fastOutput = fastTool.locator(".tool-disclosure").filter({ hasText: "Output" });
    await slowOutput.locator("summary").click();
    await fastOutput.locator("summary").click();

    await slowOutput
      .locator(".tool-disclosure-body")
      .filter({ hasText: "slow-2" })
      .waitFor({ state: "visible" });
    await fastOutput
      .locator(".tool-disclosure-body")
      .filter({ hasText: "fast-2" })
      .waitFor({ state: "visible" });

    assert.equal((await slowOutput.locator(".tool-disclosure-body").textContent())?.includes("fast-"), false);
    assert.equal((await fastOutput.locator(".tool-disclosure-body").textContent())?.includes("slow-"), false);

    await page
      .locator(".markdown-rendered")
      .filter({ hasText: "Tool output received." })
      .waitFor({ state: "visible" });
  });
}

async function main() {
  await verifyModelPicker();
  console.log("ok - loads split frontend assets and status-driven model picker");
  await verifyPromptSubmission();
  console.log("ok - submits a prompt through the standalone web server against the fake provider");
  await verifyFlatStreamingToolTranscript();
  console.log("ok - renders flat streaming tool transcript with foldable script and output");
  await verifyConcurrentToolOutputAttribution();
  console.log("ok - keeps concurrent tool output attributed to each tool call");
}

await main();
