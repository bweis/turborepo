// Walking up with `..` through an alias still reaches into another package by path
import { blackbeard } from "@/../../packages/another/index.jsx";
// tsconfig path alias into a declared workspace dependency (`another`) is allowed
import { blackbead } from "!";

export interface Pirate {
  ship: string;
}
