using System;
using System.IO;
using System.Threading.Tasks;
using AirBridge.Windows.Core;
using Windows.Storage;

namespace AirBridge.Windows.Models;

/// <summary>A file chosen by the picker or dropped onto the radar.</summary>
public sealed class StagedFile : ObservableObject
{
    private string _path;

    private StagedFile(StorageFile? file, string path, string name, long size)
    {
        File = file;
        _path = path;
        Name = name;
        Size = size;
    }

    public StorageFile? File { get; }

    public string Path
    {
        get => _path;
        private set => SetProperty(ref _path, value);
    }

    public string Name { get; }

    public long Size { get; }

    public string SizeText => Format.Bytes(Size);

    public string Glyph
    {
        get
        {
            var extension = System.IO.Path.GetExtension(Name).ToLowerInvariant();
            return extension switch
            {
                ".png" or ".jpg" or ".jpeg" or ".gif" or ".webp" or ".heic" or ".bmp" or ".tif" or ".tiff" => "\uEB9F",
                ".mp4" or ".mov" or ".mkv" or ".avi" or ".webm" or ".m4v" => "\uE714",
                ".mp3" or ".wav" or ".flac" or ".m4a" or ".aac" or ".ogg" => "\uE8D6",
                ".zip" or ".7z" or ".rar" or ".tar" or ".gz" => "\uE7B8",
                ".pdf" => "\uEA90",
                _ => "\uE8A5",
            };
        }
    }

    public string AutomationName => $"{Name}, {SizeText}";

    public static async Task<StagedFile> FromStorageFileAsync(StorageFile file)
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

        return new StagedFile(file, file.Path, file.Name, size);
    }

    public static StagedFile FromPath(string path)
    {
        var info = new FileInfo(path);
        return new StagedFile(null, path, info.Name, info.Exists ? info.Length : 0);
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
}
