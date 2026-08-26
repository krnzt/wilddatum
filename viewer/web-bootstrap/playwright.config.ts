import {defineConfig} from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  timeout: 120_000,
  expect: {timeout: 30_000},
  fullyParallel: false,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? [["github"], ["html", {open: "never"}]] : "list",
  use: {
    headless: true,
    viewport: {width: 1280, height: 800},
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "retain-on-failure",
    launchOptions: {args: ["--use-angle=swiftshader"]}
  }
});
