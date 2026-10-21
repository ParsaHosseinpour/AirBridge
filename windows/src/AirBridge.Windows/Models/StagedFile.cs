using System;
using System.IO;
using System.Threading.Tasks;
using AirBridge.Windows.Core;
using Windows.Storage;

namespace AirBridge.Windows.Models;

/// <summary>A file chosen by the picker, a folder walk, or a drop onto the radar.</summary>
public sealed class StagedFile : ObservableObject
{
    private string _path;
    private string _relativePath;

    private StagedFile(StorageFile? file, string path, string name, long size, string relativePath)
    {
        File = file;
        _path = path;
        Name = name;
        Size = size;
        _relativePath = Normalize(relativePath, name);
    }

    public StorageFile? File { get; }

    public string Path
    {
        get => _path;
        private set => SetProperty(ref _path, value);
    }

    public string Name { get; }

    public long Size { get; }

    /// <summary>
    /// The `/`-separated path carried in the batch manifest. A folder
    /// selection preserves the folder prefix (`album/one.jpg`); a single file
    /// uses its own name. This is never the timestamped outgoing-cache name.
    /// </summary>
    public string RelativePath
    {
        get => _relativePath;
        private set
        {
            if (SetProperty(ref _relativePath, value))
            {
                OnPropertiesChanged(nameof(Folder), nameof(DisplayPath), nameof(AutomationName));
            }
        }
    }

    /// <summary>The containing folders inside the selected folder, if any.</summary>
    public string? Folder
    {
        get
        {
            var slash = RelativePath.LastIndexOf('/');
            return slash > 0 ? RelativePath[..slash] : null;
        }
    }

    public string DisplayPath => RelativePath;

    public string SizeText => Format.Bytes(Size);

    public string Glyph => FileKinds.GlyphFor(Name);

    public string AutomationName =>
        Folder is null ? $"{Name}, {SizeText}" : $"{Name} in {Folder}, {SizeText}";

    public static async Task<StagedFile> FromStorageFileAsync(StorageFile file, string? relativePath = null)
    {
        long size = 0;
        try
        {
            var properties = await file.GetBasicPropertiesAsync();
            size = (long)properties.Size;
        }
        catch
        {
            // A provider may not expose properties; the transfer still works.
        }

        return new StagedFile(file, file.Path, file.Name, size, relativePath ?? file.Name);
    }

    public static StagedFile FromPath(string path, string? relativePath = null)
    {
        var info = new FileInfo(path);
        return new StagedFile(null, path, info.Name, info.Exists ? info.Length : 0, relativePath ?? info.Name);
    }

    /// <summary>
    /// Returns a filesystem path the Rust transport can open. Picker results
    /// from virtual providers have no path, so those are copied to the app's
    /// local outgoing cache first.
    /// </summary>
    public async Task<string> EnsureLocalPathAsync()
    {
        if (!string.IsNullOrWhiteSpace(Path) && System.IO.File.Exists(Path))
        {
            return Path;
        }

        if (File is null)
        {
            throw new FileNotFoundException("The file is no longer available.", Name);
        }

        var directory = System.IO.Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
            "AirBridge",
            "outgoing");
        Directory.CreateDirectory(directory);

        var destination = System.IO.Path.Combine(
            directory,
            $"{DateTime.Now:yyyyMMddHHmmss}-{Guid.NewGuid():N}-{Format.SanitizeFileName(Name)}");

        await Task.Run(async () =>
        {
            using var source = await File.OpenStreamForReadAsync();
            using var target = System.IO.File.Create(destination);
            await source.CopyToAsync(target);
        });

        Path = destination;
        return destination;
    }

    private static string Normalize(string? relativePath, string name)
    {
        var raw = string.IsNullOrWhiteSpace(relativePath) ? name : relativePath;
        var segments = raw
            .Replace('\\', '/')
            .Split('/', StringSplitOptions.RemoveEmptyEntries);

        return segments.Length == 0 ? name : string.Join('/', segments);
    }
}
