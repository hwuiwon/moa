import {
  RouterProvider,
  createHashRouter,
} from "react-router-dom";

import { AppLayout, HomeRedirect } from "@/components/layout/app-layout";
import { TooltipProvider } from "@/components/ui/tooltip";
import { ChatView } from "@/views/chat-view";
import { MemoryView } from "@/views/memory-view";
import { SettingsView } from "@/views/settings-view";

const router = createHashRouter([
  {
    element: <AppLayout />,
    children: [
      { index: true, element: <HomeRedirect /> },
      { path: "/chat", element: <ChatView /> },
      { path: "/chat/:sessionId", element: <ChatView /> },
      { path: "/memory", element: <MemoryView /> },
      { path: "/settings", element: <SettingsView /> },
    ],
  },
]);

function App() {
  return (
    <TooltipProvider delay={150}>
      <RouterProvider router={router} />
    </TooltipProvider>
  );
}

export default App;
