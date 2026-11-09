const agent = {
  submit(data) {
    const status = (text, sent) => {
      document.getElementById("agent-status")?.remove();
      const colors = sent ? "background:#0f2416;color:#9df0bd;border:1px solid #1f5133" : "background:#2a1012;color:#ffb4b4;border:1px solid #5c2226";
      document.body.insertAdjacentHTML(
        "beforeend",
        `<p id="agent-status" role="status" style="position:fixed;top:16px;left:50%;transform:translateX(-50%);z-index:2147483647;margin:0;padding:8px 14px;border-radius:999px;${colors};font:13px -apple-system,Inter,sans-serif">${text}</p>`,
      );
    };
    return fetch(location.pathname, { method: "POST", body: JSON.stringify(data) }).then(
      (response) => {
        if (response.ok) return status("Sent. Back to the terminal.", true);
        const reason = response.status === 409 ? "the agent is not waiting for this page" : `the agent answered ${response.status}`;
        status(`Not sent: ${reason}.`, false);
        throw new Error(reason);
      },
      (error) => {
        status("Not sent: the session that served this page is not running.", false);
        throw error;
      },
    );
  },
};
