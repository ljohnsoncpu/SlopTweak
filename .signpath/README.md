# SignPath setup (after the SignPath Foundation approves the project)

The release workflow already has the two signing steps; they're skipped until
the variable below is set.

1. In SignPath (organization created by the Foundation for this project):
   - **Trusted build system:** link the predefined *GitHub.com* system to the
     project. Optionally install the SignPath GitHub App on
     `ljohnsoncpu/SlopTweak`.
   - **Artifact configurations:** create two, with these slugs and the XML in
     this folder:
     - `app-exe`: [`artifact-configurations/app-exe.xml`](artifact-configurations/app-exe.xml)
     - `installer`: [`artifact-configurations/installer.xml`](artifact-configurations/installer.xml)

     Both require a `version` parameter (release.yml passes the release
     version) and restrict the file's ProductName to `SlopTweak` and its
     ProductVersion to that version.
   - **Signing policy:** the Foundation's release-signing policy, with manual
     approval by the Approver listed in the README. Note its slug.
   - **API token:** a CI user with *submitter* rights on the project.
2. In GitHub (Settings → Secrets and variables → Actions):
   - secret `SIGNPATH_API_TOKEN`
   - variables `SIGNPATH_ORGANIZATION_ID`, `SIGNPATH_PROJECT_SLUG`,
     `SIGNPATH_POLICY_SLUG`
3. Run the Release workflow by hand (dry run) and approve both requests. The
   workflow checks that each signed file has a valid Authenticode signature
   before using it.
4. Remove "*(Application pending…)*" from the README's code signing section.

Why two requests: the installer contains the app exe, so the exe must be signed
before `tauri bundle` packs it; the installer is signed after. The updater
signature (`.sig`) is made last, over the final signed installer.

SmartScreen: a new OV certificate still has to build reputation from downloads,
so early signed releases may show a warning for a while.
