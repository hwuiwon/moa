import { RouterProvider } from "@tanstack/react-router";
import { TooltipProvider } from "@/components/ui/tooltip";
import { router } from "@/router";

function App() {
  return (
    <TooltipProvider delay={150}>
      <RouterProvider router={router} />
    </TooltipProvider>
  );
}

export default App;
