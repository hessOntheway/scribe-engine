import React from "react";
import ReactDOM from "react-dom/client";
import mermaid from "mermaid";
import "./styles.css";
import { App } from "./App";

mermaid.initialize({
  startOnLoad: false,
  theme: "base",
  securityLevel: "loose",
  themeVariables: {
    background: "#11182d",
    primaryColor: "#18243f",
    primaryTextColor: "#e6eefb",
    primaryBorderColor: "#6cb6ff",
    lineColor: "#91a3bd",
    secondaryColor: "#11192d",
    tertiaryColor: "#0d1425",
    fontFamily: "SF Pro Text, Segoe UI, sans-serif",
  },
});

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
