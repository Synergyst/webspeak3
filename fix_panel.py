import re

file_path = "/home/uc/Sources/webspeak3/web/src/App.tsx"
with open(file_path, "r", encoding="utf-8") as f:
    content = f.read()

start_idx = content.find("function AnwendungPanel({")
if start_idx == -1:
    print("Could not find AnwendungPanel")
    exit(1)

brace_count = 0
end_idx = -1
for i in range(start_idx, len(content)):
    if content[i] == "{":
        brace_count += 1
    elif content[i] == "}":
        brace_count -= 1
        if brace_count == 0:
            end_idx = i + 1
            break

if end_idx == -1:
    print("Could not find closing brace")
    exit(1)

replacement = """function AnwendungPanel({
  regenerateIdentity,
  onRegenerateIdentityChange,
}: {
  regenerateIdentity: boolean;
  onRegenerateIdentityChange: (v: boolean) => void;
}) {
  const t = useT();
  const { langPref, setLangPref } = useLanguage();
  return (
    <>
      <h3>{t("app.title")}</h3>
      <p className="ts-options-subtitle">{t("app.subtitle")}</p>
      <label className="ts-options-field">
        {t("app.language")}
        <select value={langPref} onChange={(e) => setLangPref(e.target.value as LangPref)}>
          <option value="auto">{t("app.language.auto")}</option>
          <option value="de">{t("app.language.de")}</option>
          <option value="en">{t("app.language.en")}</option>
          <option value="zh-CN">{t("app.language.zh-CN")}</option>
          <option value="fa">{t("app.language.fa")}</option>
        </select>
      </label>
      <label className="ts-options-checkbox">
        <input
          type="checkbox"
          checked={regenerateIdentity}
          onChange={(e) => onRegenerateIdentityChange(e.target.checked)}
        />
        {t("app.regenerateIdentity")}
      </label>
      <p className="ts-options-hint">
        <a href="https://hosted.weblate.org/projects/webspeak3/" target="_blank" rel="noreferrer">
          {t("app.language.helpTranslate")}
        </a>
      </p>
    </>
  );
}
"""

new_content = content[:start_idx] + replacement + content[end_idx:]
with open(file_path, "w", encoding="utf-8") as f:
    f.write(new_content)
print("Successfully fixed AnwendungPanel")
