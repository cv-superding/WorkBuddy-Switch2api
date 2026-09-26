import { Component, type ErrorInfo, type ReactNode } from "react";
import { AlertTriangle, RotateCcw } from "lucide-react";

import { Button } from "@/components/ui/button";

interface Props {
  children: ReactNode;
  /** 出错时面板上显示的模块名，便于定位是哪一块崩了。 */
  label?: string;
}

interface State {
  error: Error | null;
}

/**
 * 页面级错误边界：捕获子树**渲染期**抛出的致命错误，显示可恢复的错误面板，
 * 而不是让整个窗口永久白屏。
 *
 * 为什么必须有：React 一旦在渲染期抛错且没有错误边界，会卸载整棵树，
 * 用户看到的就是一个纯白窗口 —— 没有报错、没法重试，只能杀进程。
 * 已知能触发这种情况的一类数据是「后端把对象透传到前端，被当作 React 子节点渲染」，
 * 例如 2026-09-23 WorkBuddy 更新后鉴权文件里的加密信封
 * `{ $wbEncrypted, envelope }` → React #31（Objects are not valid as a React child）。
 * 后端已对账号元数据做收敛，这里再加一层，任何**将来**的数据异常都能自救。
 *
 * 注意：只能用 class 组件（React 规定 hook 无法捕获渲染错误）。
 */
export class PageErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    // 打到控制台，用户按 F12 就能把堆栈贴出来。
    console.error("[PageErrorBoundary] 页面渲染崩溃：", error, info.componentStack);
  }

  private handleRetry = () => {
    this.setState({ error: null });
  };

  private handleReload = () => {
    window.location.reload();
  };

  render() {
    const { error } = this.state;
    if (!error) return this.props.children;

    return (
      <div
        role="alert"
        className="flex min-h-[60vh] flex-col items-center justify-center gap-4 p-10 text-center"
      >
        <div className="flex w-full max-w-md flex-col items-center">
          <AlertTriangle className="size-10 text-destructive" aria-hidden="true" />
          <h2 className="mt-3 text-lg font-semibold text-foreground">
            {this.props.label ? `${this.props.label}页面渲染出错` : "页面渲染出错"}
          </h2>
          <p className="mt-2 text-sm leading-6 text-muted-foreground">
            本页遇到了意外的渲染错误。先点「重试」；若反复出现，按 F12 打开控制台把红色堆栈截下来反馈。
          </p>
          <pre className="mt-3 max-h-44 w-full overflow-auto rounded-md bg-muted p-3 text-left text-xs whitespace-pre-wrap text-muted-foreground">
            {String(error.message || error)}
          </pre>
          <div className="mt-4 flex items-center gap-2">
            <Button onClick={this.handleRetry} className="gap-1.5">
              <RotateCcw className="size-4" />
              重试
            </Button>
            <Button variant="outline" onClick={this.handleReload}>
              重新载入界面
            </Button>
          </div>
        </div>
      </div>
    );
  }
}
