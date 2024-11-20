#include "MainWindow.h"
#include "./ui_mainwindow.h"
#include <QPixmap>
#include <QKeyEvent>
#include <QFileInfo>
#include <QScreen>

MainWindow::MainWindow(QWidget *parent, ImageController *controller)
    : QMainWindow(parent)
    , ui(new Ui::MainWindow)
    , imageLabel(new QLabel(this))
    , imageController(controller)  // Initialize controller
{
    ui->setupUi(this);

    // Get the screen dimensions
    QRect screenGeometry = screen()->geometry();
    int screenWidth = screenGeometry.width();
    int screenHeight = screenGeometry.height();

    move(0,0);

    // Display the first image
    QPixmap pixmap(QString::fromStdString(imageController->getCurrentFile()));

    if (!pixmap.isNull()) {
        imageLabel->setPixmap(pixmap.scaled(screenWidth, screenHeight, Qt::KeepAspectRatio));
        imageLabel->setAlignment(Qt::AlignCenter);
        setCentralWidget(imageLabel);

        QFileInfo fileInfo(QString::fromStdString(imageController->getCurrentFile()));
        setWindowTitle(fileInfo.fileName());
    } else {
        imageLabel->setText("Failed to load image");
        imageLabel->setAlignment(Qt::AlignCenter);
        setCentralWidget(imageLabel);
    }
}

MainWindow::~MainWindow() {
    delete ui;
}

void MainWindow::keyPressEvent(QKeyEvent *event) {
    // Get the screen dimensions
    QRect screenGeometry = screen()->geometry();
    int screenWidth = screenGeometry.width();
    int screenHeight = screenGeometry.height();

    switch (event->key()) {
    case Qt::Key_Left:
        imageController->previousImage();
        break;
    case Qt::Key_Right:
        imageController->nextImage();
        break;
    default:
        break;
    }

    // Update the displayed image
    QPixmap pixmap(QString::fromStdString(imageController->getCurrentFile()));
    if (!pixmap.isNull()) {
        imageLabel->setPixmap(pixmap.scaled(screenWidth, screenHeight, Qt::KeepAspectRatio));
        QFileInfo fileInfo(QString::fromStdString(imageController->getCurrentFile()));
        setWindowTitle(fileInfo.fileName());
    } else {
        imageLabel->setText("Failed to load image");
    }
}
