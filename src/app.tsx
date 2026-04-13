import { RouterProvider } from "@tanstack/react-router";

import { Toaster } from "@/components/ui/sonner";
import { TooltipProvider } from "@/components/ui/tooltip";
import { router } from "@/router";

/**
 * Root React application component for the desktop shell.
 */
function App() {
  return (
    <TooltipProvider delay={150}>
      <RouterProvider router={router} />
      <Toaster richColors />
    </TooltipProvider>
  );
}

export default App;
