/** Load the shared skill from this installed package through DSH's native provider. */
import { fileURLToPath } from "node:url";
import * as filesystem from "@deepseek-ai/dsh-skill-filesystem";

export const name = "openorch";
export const inject = ["skills"];

/** Register an isolated host provider; do not depend on Web's disabled default roots. */
export function apply(ctx) {
  ctx.plugin(filesystem, {
    providerName: "openorch",
    includeDefaultRoots: false,
    customSkillDirs: [fileURLToPath(new URL("./skills", import.meta.url))],
  });
}
