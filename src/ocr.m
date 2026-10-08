#import <Foundation/Foundation.h>
#import <Vision/Vision.h>
#import <ImageIO/ImageIO.h>
#import <PDFKit/PDFKit.h>

/// Runs Vision text recognition over one image and returns the recognized text, one
/// line per observation. Returns an empty string when recognition fails.
///
/// `fast` selects Vision's fast recognizer, which is several times quicker than the
/// accurate one and good enough for search recall on most screenshots.
static NSString* ocr_cgimage(CGImageRef cgImage, BOOL fast) {
    if (!cgImage) return @"";
    
    VNRecognizeTextRequest *request = [[VNRecognizeTextRequest alloc] init];
    request.recognitionLevel = fast ? VNRequestTextRecognitionLevelFast : VNRequestTextRecognitionLevelAccurate;
    
    VNImageRequestHandler *handler = [[VNImageRequestHandler alloc] initWithCGImage:cgImage options:@{}];
    
    NSError *error = nil;
    @try {
        [handler performRequests:@[request] error:&error];
    } @catch (NSException *e) {
        // Vision can raise on a malformed image; treat that as "no text"
        return @"";
    }
    
    if (error || !request.results) return @"";
    
    NSMutableString *result = [NSMutableString string];
    for (VNRecognizedTextObservation *observation in request.results) {
        NSArray<VNRecognizedText *> *topCandidates = [observation topCandidates:1];
        if (topCandidates.count > 0) {
            [result appendFormat:@"%@\n", topCandidates.firstObject.string];
        }
    }
    
    return result;
}

/// Image pages and raster attachments are downscaled so their long side is at most this
/// many pixels before recognition. Vision's cost scales with pixel count and text stays
/// legible at this size, while a full-resolution retina screenshot is both slow and
/// the usual trigger for ImageIO running out of memory.
static const CGFloat MAX_OCR_PIXELS = 2000.0;

const char* perform_ocr(const char* image_path, int fast_level) {
    @autoreleasepool {
        @try {
            BOOL fast = fast_level != 0;
            NSString *path = [NSString stringWithUTF8String:image_path];
            if (!path) return strdup("");
            NSURL *url = [NSURL fileURLWithPath:path];
            
            if ([path.pathExtension.lowercaseString isEqualToString:@"pdf"]) {
                PDFDocument *doc = [[PDFDocument alloc] initWithURL:url];
                if (!doc) return strdup("");
                
                // An encrypted document exposes nothing readable without its password
                if (doc.isLocked && ![doc unlockWithPassword:@""]) return strdup("");
                
                // Reading an embedded text layer is instant, while rendering and recognizing a
                // page is not, so only a limited number of image-only pages get the Vision pass
                const NSUInteger MAX_SCANNED_PAGES = 8;
                NSUInteger scannedOcrCount = 0;
                
                NSMutableString *result = [NSMutableString string];
                
                for (NSUInteger i = 0; i < doc.pageCount; i++) {
                    // Every page leaves behind a rendered bitmap and a pile of Vision buffers,
                    // so they are drained before the next page starts
                    @autoreleasepool {
                        // The whole per-page body is guarded: a page that throws is skipped
                        // instead of costing the rest of the document
                        @try {
                            PDFPage *page = [doc pageAtIndex:i];
                            if (page) {
                                NSString *pageText = page.string;
                                BOOL hasText = pageText != nil
                                    && [pageText stringByTrimmingCharactersInSet:[NSCharacterSet whitespaceAndNewlineCharacterSet]].length > 0;
                                
                                if (hasText) {
                                    [result appendFormat:@"%@\n", pageText];
                                } else if (scannedOcrCount < MAX_SCANNED_PAGES) {
                                    CGRect pageRect = [page boundsForBox:kPDFDisplayBoxMediaBox];
                                    
                                    // 108 dpi keeps small type legible to Vision while roughly
                                    // halving inference time per page; the long side is capped so
                                    // an oversized page cannot allocate an enormous bitmap
                                    CGFloat scale = 1.5;
                                    CGFloat maxDim = MAX(pageRect.size.width, pageRect.size.height);
                                    if (maxDim * scale > MAX_OCR_PIXELS && maxDim > 0.0) {
                                        scale = MAX_OCR_PIXELS / maxDim;
                                    }
                                    size_t width = (size_t)(pageRect.size.width * scale);
                                    size_t height = (size_t)(pageRect.size.height * scale);
                                    
                                    if (width > 0 && height > 0) {
                                        CGColorSpaceRef colorSpace = CGColorSpaceCreateDeviceRGB();
                                        CGContextRef ctx = CGBitmapContextCreate(NULL, width, height, 8, width * 4, colorSpace,
                                                                                 kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big);
                                        CGColorSpaceRelease(colorSpace);
                                        
                                        if (ctx) {
                                            // White background first, otherwise transparent pixels read as noise
                                            CGContextSetRGBFillColor(ctx, 1.0, 1.0, 1.0, 1.0);
                                            CGContextFillRect(ctx, CGRectMake(0, 0, (CGFloat)width, (CGFloat)height));
                                            
                                            CGContextSaveGState(ctx);
                                            CGContextScaleCTM(ctx, scale, scale);
                                            [page drawWithBox:kPDFDisplayBoxMediaBox toContext:ctx];
                                            CGContextRestoreGState(ctx);
                                            
                                            CGImageRef rendered = CGBitmapContextCreateImage(ctx);
                                            // The image holds its own copy of the pixels, so the
                                            // context is released the moment it is captured
                                            CGContextRelease(ctx);
                                            
                                            if (rendered) {
                                                NSString *ocrText = ocr_cgimage(rendered, fast);
                                                if (ocrText.length > 0) {
                                                    [result appendFormat:@"%@\n", ocrText];
                                                }
                                                CGImageRelease(rendered);
                                                scannedOcrCount++;
                                            }
                                        }
                                    }
                                }
                            }
                        } @catch (NSException *e) {
                            // Skip the bad page and keep going
                        }
                    }
                }
                
                return strdup([result UTF8String]);
            }
            
            CGImageSourceRef imageSource = CGImageSourceCreateWithURL((__bridge CFURLRef)url, NULL);
            if (!imageSource) return strdup("");
            
            // ImageIO can already tell that some files are not images at all, or are
            // truncated past recovery; bail before asking it to decode them
            CGImageSourceStatus status = CGImageSourceGetStatus(imageSource);
            if (status == kCGImageStatusUnknownType || status == kCGImageStatusInvalidData
                || CGImageSourceGetCount(imageSource) == 0) {
                CFRelease(imageSource);
                return strdup("");
            }
            
            // Decode straight to a downscaled bitmap instead of materializing the full
            // image: cheaper for Vision, far less memory for oversized screenshots, and
            // the EXIF orientation is applied so rotated photos read correctly
            NSDictionary *thumbnailOptions = @{
                (__bridge NSString *)kCGImageSourceCreateThumbnailFromImageAlways: @YES,
                (__bridge NSString *)kCGImageSourceCreateThumbnailWithTransform: @YES,
                (__bridge NSString *)kCGImageSourceThumbnailMaxPixelSize: @((NSInteger)MAX_OCR_PIXELS),
            };
            CGImageRef cgImage = CGImageSourceCreateThumbnailAtIndex(imageSource, 0, (__bridge CFDictionaryRef)thumbnailOptions);
            
            // A few formats refuse the thumbnail path; fall back to a full decode for them
            if (!cgImage) {
                cgImage = CGImageSourceCreateImageAtIndex(imageSource, 0, NULL);
            }
            if (!cgImage) {
                CFRelease(imageSource);
                return strdup("");
            }
            
            NSString *result = ocr_cgimage(cgImage, fast);
            
            CGImageRelease(cgImage);
            CFRelease(imageSource);
            
            return strdup([result UTF8String]);
        } @catch (NSException *e) {
            // A malformed or corrupted file must never abort the indexing worker
            return strdup("");
        }
    }
}

void free_ocr_string(char* str) {
    if (str) {
        free(str);
    }
}