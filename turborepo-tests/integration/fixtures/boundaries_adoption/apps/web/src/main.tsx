import { createRouter } from "@tanstack/react-router";
import { routeTree } from "@/routeTree.gen";
import { Button } from "@ui/button";
import { formatDate } from "@utils/date";

export const router = createRouter({ routeTree });

export function App() {
  return Button({ label: formatDate(new Date()) });
}
