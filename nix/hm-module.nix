self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.quillway;
  toml = pkgs.formats.toml { };
  configFile = toml.generate "quillway-config.toml" cfg.settings;
in
{
  options.services.quillway = {
    enable = lib.mkEnableOption "Quillway, a shortcut-summoned rewrite popup backed by local models";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "quillway.packages.\${system}.default";
      description = "The Quillway package (choose a llama.cpp backend with `.override { llamaCpp = …; }`).";
    };

    settings = lib.mkOption {
      inherit (toml) type;
      default = { };
      example = lib.literalExpression ''
        {
          model.active = "gemma-4-e4b";
          ui = { theme = "dark"; opacity = 0.78; client_shadow = false; };
          behavior.recent_secs = 30;
        }
      '';
      description = ''
        Written to {file}`$XDG_CONFIG_HOME/quillway/config.toml`. See the README for every key.
        Unknown keys are rejected so typos surface at daemon start.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."quillway/config.toml" = lib.mkIf (cfg.settings != { }) { source = configFile; };

    systemd.user.services.quillway = {
      Unit = {
        Description = "Quillway rewrite popup daemon";
        PartOf = [ "graphical-session.target" ];
        After = [ "graphical-session.target" ];
        X-Restart-Triggers = lib.optional (cfg.settings != { }) "${configFile}";
      };
      Service = {
        ExecStart = "${lib.getExe cfg.package} daemon";
        Restart = "on-failure";
        RestartSec = 2;
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };
  };
}
