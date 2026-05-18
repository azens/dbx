import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import assert from "node:assert/strict";
import test from "node:test";

const connectionDialogSource = readFileSync("apps/desktop/src/components/connection/ConnectionDialog.vue", "utf8");
const databaseIconSource = readFileSync("apps/desktop/src/components/icons/DatabaseIcon.vue", "utf8");

const expectedIconNames = [
  "databricks",
  "saphana",
  "teradata",
  "vertica",
  "firebird",
  "exasol",
  "gbase",
  "tdsql",
  "polardb",
  "greatsql",
] as const;

test("new database profiles use dedicated icon identities", () => {
  for (const iconName of expectedIconNames) {
    assert.match(connectionDialogSource, new RegExp(`${iconName}: \\{[^}]*icon: "${iconName}"`, "s"));
    assert.match(databaseIconSource, new RegExp(`${iconName}: "${iconName}\\.webp"`));
  }
});

test("new database favicon assets exist", () => {
  for (const iconName of expectedIconNames) {
    const iconPath = join("apps/desktop/public/icons/database", `${iconName}.webp`);
    assert.equal(existsSync(iconPath), true, `${iconPath} should exist`);
  }
});
