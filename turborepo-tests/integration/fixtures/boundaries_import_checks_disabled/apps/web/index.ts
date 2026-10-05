// Each of these imports would be reported if import checks were enabled.
import { internal } from "../../packages/internal/index";
import { ui } from "@repo/ui";
import "undeclared-dependency";

export const web = [internal, ui];
